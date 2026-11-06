use super::*;
use flint_app::settings::{
    OPENCODE_GO_BASE_URL, OPENCODE_GO_KEY_ENV, OPENCODE_MODEL, OPENCODE_ZEN_BASE_URL,
    OPENCODE_ZEN_KEY_ENV, ProviderKind, Settings,
};

#[gpui_kit::test]
fn opencode_presets_stage_remote_endpoints_with_their_own_keys(cx: &mut TestAppContext) {
    for (preset, base_url, key_env) in [
        (
            "preset-opencode-zen",
            OPENCODE_ZEN_BASE_URL,
            OPENCODE_ZEN_KEY_ENV,
        ),
        (
            "preset-opencode-go",
            OPENCODE_GO_BASE_URL,
            OPENCODE_GO_KEY_ENV,
        ),
    ] {
        let ui = open(cx);
        let before = ui.read(cx, |app, _| app.settings.clone());
        ui.press(cx, "secondary-,");
        ui.click_settings(cx, preset);
        ui.read(cx, |app, cx| {
            assert_eq!(app.settings, before);
            let draft = app.settings_form.as_ref().unwrap().draft(&app.settings, cx);
            assert_eq!(draft.provider, ProviderKind::Custom);
            assert_eq!(draft.base_url, base_url);
            assert_eq!(draft.model, OPENCODE_MODEL);
            assert!(draft.subagent_model.is_empty());
            assert_eq!(draft.api_key_env, key_env);
            assert!(draft.api_key_file.is_empty());
            assert!(!draft.omniroute_no_auth);
            assert_eq!(draft.approval, before.approval);
        });
        assert!(!has(&ui, cx, "settings-omniroute-no-auth"));
        ui.click(cx, "settings-cancel");
        assert_eq!(ui.read(cx, |app, _| app.settings.clone()), before);
    }
}

#[gpui_kit::test]
fn switching_between_opencode_presets_updates_url_and_key_name(cx: &mut TestAppContext) {
    let ui = open(cx);
    ui.press(cx, "secondary-,");
    ui.click_settings(cx, "preset-opencode-zen");
    ui.click_settings(cx, "preset-opencode-go");
    ui.read(cx, |app, cx| {
        let draft = app.settings_form.as_ref().unwrap().draft(&app.settings, cx);
        assert_eq!(draft.base_url, OPENCODE_GO_BASE_URL);
        assert_eq!(draft.api_key_env, OPENCODE_GO_KEY_ENV);
    });
}

#[gpui_kit::test]
fn opencode_preset_saves_without_borrowing_ambient_provider_keys(cx: &mut TestAppContext) {
    let ui = open(cx);
    ui.press(cx, "secondary-,");
    ui.click_settings(cx, "preset-opencode-go");
    ui.click(cx, "settings-save");
    ui.read(cx, |app, _| {
        let saved = Settings::load(&app.home, &KeySources::none());
        assert_eq!(saved.provider, ProviderKind::Custom);
        assert_eq!(saved.base_url, OPENCODE_GO_BASE_URL);
        assert_eq!(saved.api_key_env, OPENCODE_GO_KEY_ENV);
        let sources = KeySources {
            env: |name| {
                matches!(name, "FLINT_API_KEY" | "OPENAI_API_KEY")
                    .then(|| "wrong-provider-fixture".into())
            },
            legacy_key_file: None,
        };
        assert!(saved.resolve_key(None, &sources).is_none());
        assert!(
            flint_app::engine::config_for(
                &app.session().workspace,
                &saved,
                None,
                &sources,
                app.approval,
            )
            .is_err()
        );
        assert!(app.session().ops.is_none());
    });
}

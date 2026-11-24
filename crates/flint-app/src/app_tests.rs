use std::path::PathBuf;

use gpui_kit::{TestAppContext, WindowHandle};

use crate::app::FlintApp;
use crate::app::Options;
use crate::app::SessionGrouping;
use crate::session::Session;
use crate::session::TriageSection;
use crate::session_groups::GroupKey;
use crate::settings::KeySources;

fn options(root: &std::path::Path) -> Options {
    Options {
        home: Some(root.join("home")),
        workspace: Some(root.join("ws")),
        key_sources: Some(KeySources::none()),
        skip_permission_choice: true,
        ..Default::default()
    }
}

fn fixture(cx: &mut TestAppContext) -> (tempfile::TempDir, WindowHandle<FlintApp>) {
    cx.update(crate::init);
    let root = tempfile::tempdir().unwrap();
    let window = cx.add_window(|window, cx| FlintApp::new(options(root.path()), window, cx));
    (root, window)
}

fn listed_session(uid: u64) -> Session {
    let mut session = Session::new(uid, PathBuf::from("/fixture"));
    let change = session.view.push_user(format!("session {uid}"));
    session.apply(change);
    session
}

#[gpui_kit::test]
fn navigation_history_walks_back_and_forward_and_skips_vanished(cx: &mut TestAppContext) {
    let (_root, window) = fixture(cx);
    let _ = window.update(cx, |app, _, cx| {
        app.sessions = (10..13).map(listed_session).collect();
        app.active = 0;
        assert_eq!(app.sessions[app.active].uid, 10);

        app.open_session(1, cx);
        app.open_session(2, cx);
        assert_eq!(app.nav_back, vec![10, 11]);
        assert!(app.nav_forward.is_empty());

        app.nav_back(cx);
        assert_eq!(app.sessions[app.active].uid, 11);
        assert_eq!(app.nav_forward, vec![12]);
        app.nav_back(cx);
        assert_eq!(app.sessions[app.active].uid, 10);
        assert_eq!(app.nav_forward, vec![12, 11]);

        app.nav_forward(cx);
        assert_eq!(app.sessions[app.active].uid, 11);
        assert_eq!(app.nav_back, vec![10]);

        // A fresh selection after going back clears forward history.
        app.open_session(2, cx);
        assert_eq!(app.sessions[app.active].uid, 12);
        assert!(app.nav_forward.is_empty());

        // Sessions archived since the visit are drained without landing.
        app.nav_back = vec![11, 9_999];
        app.nav_forward.clear();
        app.nav_back(cx);
        assert_eq!(app.sessions[app.active].uid, 11);
        assert_eq!(app.nav_back, Vec::<u64>::new());
        assert_eq!(app.nav_forward, vec![12]);
    });
}

#[gpui_kit::test]
fn settling_a_session_flips_state_and_persists_the_meta(cx: &mut TestAppContext) {
    let (root, window) = fixture(cx);
    let _ = window.update(cx, |app, _, cx| {
        let dir = app.home.join("sessions").join("settle-test");
        app.sessions[0].dir = Some(dir.clone());
        app.sessions[0].settled = false;

        app.toggle_settled(0, cx);
        assert!(app.sessions[0].settled);
        assert!(app.store_error.is_none());
        assert!(crate::store::load(&dir).unwrap().0.settled);

        app.toggle_settled(0, cx);
        assert!(!app.sessions[0].settled);
        assert!(!crate::store::load(&dir).unwrap().0.settled);

        app.toggle_settled(0, cx);
        assert!(app.sessions[0].settled);
    });

    // A fresh launch restores the saved state from the session's meta.
    let reopened = cx.add_window(|window, cx| FlintApp::new(options(root.path()), window, cx));
    let _ = reopened.update(cx, |app, _, _| {
        let restored = app
            .sessions
            .iter()
            .find(|session| {
                session.dir.as_ref() == Some(&root.path().join("home/sessions/settle-test"))
            })
            .expect("session restored from disk");
        assert!(restored.settled);
    });
}

#[gpui_kit::test]
fn old_meta_without_settled_still_loads(cx: &mut TestAppContext) {
    let (_root, window) = fixture(cx);
    let _ = window.update(cx, |app, _, _| {
        let dir = app.home.join("sessions").join("old-meta");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"id":"old-meta","title":null,"workspace":"/fixture","created_at":1,"updated_at":2}"#,
        )
        .unwrap();
        let meta = crate::store::load(&dir).unwrap().0;
        assert!(!meta.settled);
    });
}

#[gpui_kit::test]
fn triage_groups_attention_working_settled_in_order(cx: &mut TestAppContext) {
    let (_root, window) = fixture(cx);
    let _ = window.update(cx, |app, _, cx| {
        let mut attention = listed_session(20);
        attention.unread = true;
        let mut working = listed_session(21);
        working.view.running = true;
        let settled = listed_session(22);
        app.sessions = vec![attention, working, settled];
        app.active = 0;
        app.grouping = SessionGrouping::Triage;

        let groups = app.session_groups(cx).groups;
        let labels: Vec<_> = groups.iter().map(|group| group.label.as_str()).collect();
        assert_eq!(labels, ["Attention", "Working", "Settled"]);
        assert!(matches!(
            groups[0].key,
            GroupKey::Triage(TriageSection::Attention)
        ));
        assert!(matches!(
            groups[1].key,
            GroupKey::Triage(TriageSection::Working)
        ));
        assert!(matches!(
            groups[2].key,
            GroupKey::Triage(TriageSection::Settled)
        ));
        assert_eq!(groups.iter().map(|group| group.total).sum::<usize>(), 3);
    });
}

#[test]
fn triage_grouping_settings_and_collapse_keys_round_trip() {
    assert_eq!(
        SessionGrouping::ALL.map(SessionGrouping::setting),
        ["triage", "project", "status", "agent"]
    );
    assert_eq!(
        SessionGrouping::ALL.map(SessionGrouping::label),
        ["Triage", "Project", "Status", "Agent"]
    );
    assert_eq!(
        SessionGrouping::from_setting("triage"),
        SessionGrouping::Triage
    );
    assert_eq!(
        SessionGrouping::from_setting("project"),
        SessionGrouping::Project
    );
    assert_eq!(
        SessionGrouping::from_setting("status"),
        SessionGrouping::Status
    );
    assert_eq!(
        SessionGrouping::from_setting("agent"),
        SessionGrouping::Agent
    );
    assert_eq!(
        SessionGrouping::from_setting("not-an-order"),
        SessionGrouping::Triage
    );
    assert_eq!(SessionGrouping::Triage.next(), SessionGrouping::Project);
    assert_eq!(SessionGrouping::Agent.next(), SessionGrouping::Triage);

    for section in TriageSection::ALL {
        let key = GroupKey::Triage(section);
        let saved = key.setting_key();
        assert_eq!(saved, format!("triage:{}", section.key()));
        assert_eq!(GroupKey::from_setting_key(&saved), Some(key));
    }
    assert_eq!(GroupKey::from_setting_key("triage:unknown"), None);
}

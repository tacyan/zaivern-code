use super::*;

fn input(viewport: egui::ViewportId) -> egui::RawInput {
    let mut input = egui::RawInput {
        viewport_id: viewport,
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 500.0),
        )),
        ..Default::default()
    };
    input.viewports.entry(viewport).or_default();
    input
}

#[test]
fn focus_and_ime_are_isolated_between_shell_windows() {
    let ctx = egui::Context::default();
    let child = egui::ViewportId::from_hash_of("shell-test");
    let mut root = input(egui::ViewportId::ROOT);
    root.events = vec![egui::Event::Ime(egui::ImeEvent::Preedit("変換中".into()))];
    let _ = ctx.run(root, |ctx| {
        let key = focused_terminal_key(ctx);
        ctx.data_mut(|d| d.insert_temp(key, 7u64));
        assert!(crate::keybinds::ime_blocks_shortcuts_now(ctx));
    });
    let _ = ctx.run(input(child), |ctx| {
        let key = focused_terminal_key(ctx);
        assert_eq!(ctx.data(|d| d.get_temp::<u64>(key)), None);
        assert!(!crate::keybinds::ime_blocks_shortcuts_now(ctx));
        ctx.data_mut(|d| d.insert_temp(key, 8u64));
    });
    let _ = ctx.run(input(egui::ViewportId::ROOT), |ctx| {
        let key = focused_terminal_key(ctx);
        assert_eq!(ctx.data(|d| d.get_temp::<u64>(key)), Some(7));
        assert!(crate::keybinds::ime_blocks_shortcuts_now(ctx));
        assert!(crate::keybinds::ime_blocks_shortcuts_peek(ctx));
    });
}

#[cfg(unix)]
#[test]
fn detached_view_keeps_the_same_live_pty_and_blocks_the_original_view() {
    let dir = crate::test_util::unique_temp_dir("zaivern-shell", "detach");
    let ctx = egui::Context::default();
    let mut session = Session::spawn(
        701,
        SpawnSpec {
            title: "test".into(),
            preset_name: "test".into(),
            icon: String::new(),
            command: "/bin/sh -c 'printf READY; while IFS= read -r line; do printf \"RESULT:%s\\n\" \"$line\"; done'".into(),
            cwd: dir.clone(),
            env: HashMap::from([("ZAIVERN_HOME".into(), dir.to_string_lossy().into_owned())]),
            log_path: None,
        },
        ctx.clone(),
    )
    .expect("spawn isolated shell");
    let parser = session.parser.clone();
    let pid = session.child_pid;
    let owner = egui::ViewportId::from_hash_of("shell-owner");
    session.shell_viewport = Some(owner);
    session.preedit = "未確定".into();
    let size = session.size;
    let mut original = input(egui::ViewportId::ROOT);
    original.events = vec![egui::Event::Text("must-not-send".into())];
    let theme = crate::theme::by_name("dark");
    let _ = ctx.run(original, |ctx| {
        let key = focused_terminal_key(ctx);
        ctx.data_mut(|d| d.insert_temp(key, session.id));
        egui::CentralPanel::default().show(ctx, |ui| {
            draw(ui, &mut session, &theme, 13.0, true, true, true);
        });
        assert_eq!(ctx.data(|d| d.get_temp::<u64>(key)), None);
    });
    assert_eq!(session.size, size);
    assert_eq!(session.preedit, "未確定");
    assert!(session.running());
    assert!(Arc::ptr_eq(&parser, &session.parser));
    session.preedit.clear();
    // 所有 viewport で通常の端末描画とキー入力を通す。
    let mut widget = None;
    let _ = ctx.run(input(owner), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            let response = draw(ui, &mut session, &theme, 13.0, true, true, true);
            widget = Some(response.id);
            response.request_focus();
        });
    });
    let mut typing = input(owner);
    typing.events = vec![
        egui::Event::Text("detached-input".into()),
        egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
    ];
    let _ = ctx.run(typing, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            let response = draw(ui, &mut session, &theme, 13.0, true, true, true);
            assert_eq!(Some(response.id), widget);
            assert!(response.has_focus());
        });
    });
    // ウィンドウを閉じた時と同じ表示先解除後にも、同じ子へ入力できる。
    session.shell_viewport = None;
    session.write_bytes(b"returned-input\r");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = lock_ok(&session.parser).screen().contents();
        if text.contains("RESULT:detached-input") && text.contains("RESULT:returned-input") {
            assert!(!text.contains("must-not-send"));
            break;
        }
        assert!(Instant::now() < deadline, "PTY output missing: {text}");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(session.child_pid, pid);
    assert!(session.running());
    assert!(Arc::ptr_eq(&parser, &session.parser));
    session.kill();
    drop(session);
    std::fs::remove_dir_all(dir).unwrap();
}

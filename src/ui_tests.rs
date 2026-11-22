use crate::{BandRow, MainWindow};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, SharedString, VecModel, platform::{Key, PointerEventButton, WindowEvent}};
use std::{cell::{Cell, RefCell}, rc::Rc};

fn window() -> MainWindow {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    ui.window().set_size(LogicalSize::new(1120.0, 920.0));
    ui
}

fn element(ui: &MainWindow, label: &str, role: AccessibleRole) -> ElementHandle {
    ElementHandle::find_by_accessible_label(ui, label)
        .find(|element| element.accessible_role() == Some(role))
        .unwrap_or_else(|| panic!("missing control {label}"))
}

fn click(ui: &MainWindow, position: LogicalPosition) {
    ui.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
    ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
}

fn key(ui: &MainWindow, text: impl Into<SharedString>) {
    let text = text.into();
    ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window().dispatch_event(WindowEvent::KeyReleased { text });
}

#[test]
fn double_clicking_bass_resets_only_that_lever() {
    let ui = window();
    ui.set_simple_layout(true);
    ui.set_simple_bass(6.5);
    ui.set_simple_mid(-4.5);
    ui.set_simple_treble(3.5);
    ui.set_simple_loudness(true);
    let submitted = Rc::new(Cell::new(None));
    let capture = submitted.clone();
    ui.on_change_tone(move |bass, mid, treble, loudness| capture.set(Some((bass, mid, treble, loudness))));
    ui.show().unwrap();

    let slider = element(&ui, "Bass", AccessibleRole::Slider);
    let origin = slider.absolute_position();
    let size = slider.size();
    let position = LogicalPosition::new(origin.x + size.width / 2.0, origin.y + 10.0 + (size.height - 20.0) * 5.5 / 24.0);
    for _ in 0..2 {
        click(&ui, position);
    }
    assert_eq!(submitted.get(), Some((0.0, -4.5, 3.5, true)));
    assert_eq!(ui.get_simple_bass(), 0.0);
    assert_eq!(ui.get_simple_mid(), -4.5);
}

#[test]
fn simple_tone_levers_submit_half_decibel_steps() {
    let ui = window();
    ui.set_simple_layout(true);
    let submitted = Rc::new(Cell::new(None));
    let capture = submitted.clone();
    ui.on_change_tone(move |bass, mid, treble, _| capture.set(Some((bass, mid, treble))));
    ui.show().unwrap();

    element(&ui, "Bass", AccessibleRole::Slider).invoke_accessible_increment_action();
    element(&ui, "Mid", AccessibleRole::Slider).invoke_accessible_decrement_action();
    assert_eq!(submitted.get(), Some((0.5, -0.5, 0.0)));

    let treble = element(&ui, "Treble", AccessibleRole::Slider);
    let origin = treble.absolute_position();
    let size = treble.size();
    click(&ui, LogicalPosition::new(
        origin.x + size.width / 2.0,
        origin.y + 10.0 + (size.height - 20.0) * 8.5 / 24.0,
    ));
    assert_eq!(submitted.get(), Some((0.5, -0.5, 3.5)));
}

#[test]
fn band_toggle_does_not_submit_uncommitted_number_fields() {
    let ui = window();
    ui.set_bands(Rc::new(VecModel::from(vec![BandRow {
        frequency: "1000".into(),
        q: "1.0".into(),
        enabled: true,
        ..BandRow::default()
    }])).into());
    let number_edits = Rc::new(Cell::new(0));
    let count = number_edits.clone();
    ui.on_edit_band_number(move |_, _, _| count.set(count.get() + 1));
    let toggled = Rc::new(Cell::new(None));
    let capture = toggled.clone();
    ui.on_edit_band_enabled(move |index, enabled| capture.set(Some((index, enabled))));
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(1120.0, 1100.0));

    let frequency = element(&ui, "Band 1 frequency", AccessibleRole::TextInput);
    frequency.set_accessible_value("incomplete");
    assert_eq!(frequency.accessible_value().unwrap(), "incomplete");
    element(&ui, "Band 1 enabled", AccessibleRole::Checkbox).invoke_accessible_default_action();
    assert_eq!(toggled.get(), Some((0, false)));
    assert_eq!(number_edits.get(), 0);
}

#[test]
fn balance_controls_share_a_row_and_adjust_in_small_steps() {
    let ui = window();
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(860.0, 1060.0));
    let heading = ElementHandle::find_by_element_id(&ui, "MainWindow::balance-heading").next().unwrap();
    let readout = ElementHandle::find_by_element_id(&ui, "MainWindow::balance-readout").next().unwrap();
    let slider = element(&ui, "Balance", AccessibleRole::Slider);
    let center = element(&ui, "Center", AccessibleRole::Button);
    let controls = [&heading, &readout, &slider, &center];
    for pair in controls.windows(2) {
        assert!(pair[0].absolute_position().x < pair[1].absolute_position().x);
    }
    let center_y = |control: &ElementHandle| control.absolute_position().y + control.size().height / 2.0;
    assert!(controls.iter().all(|control| (center_y(control) - center_y(&heading)).abs() <= 1.0));
    slider.invoke_accessible_increment_action();
    assert!((ui.get_balance() - 0.05).abs() < 1e-6);
}

#[test]
fn preamp_spinbox_steps_by_005_and_auto_headroom_locks_controls() {
    let ui = window();
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(860.0, 1060.0));
    let submitted = Rc::new(Cell::new(0.0));
    let capture = submitted.clone();
    ui.on_change_preamp(move |value| capture.set(value));
    let slider = element(&ui, "Preamp", AccessibleRole::Slider);
    let plus = element(&ui, "Increase preamp", AccessibleRole::Button);
    let entry = element(&ui, "Preamp in dB", AccessibleRole::TextInput);
    let headroom = element(&ui, "Auto Headroom", AccessibleRole::Checkbox);
    assert!(headroom.absolute_position().x < entry.absolute_position().x);
    plus.invoke_accessible_default_action();
    assert!((submitted.get() - 0.05).abs() < 1e-6);
    slider.invoke_accessible_increment_action();
    assert!((ui.get_preamp() - 0.05).abs() < 1e-6);
    ui.set_auto_headroom(true);
    assert_eq!(slider.accessible_enabled(), Some(false));
    assert_eq!(plus.accessible_enabled(), Some(false));
    assert_eq!(entry.accessible_enabled(), Some(false));
}

#[test]
fn eq_graph_height_adjusts_the_plot_in_place() {
    let ui = window();
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(860.0, 1080.0));
    let plot = ElementHandle::find_by_element_id(&ui, "MainWindow::plot").next().unwrap();
    let slider = element(&ui, "EQ graph height", AccessibleRole::Slider);
    assert_eq!(plot.size().height, 260.0);
    let point = slider.absolute_position();
    let size = slider.size();
    let start = LogicalPosition::new(point.x + size.width / 2.0, point.y + size.height / 2.0);
    let end = LogicalPosition::new(start.x, start.y + 40.0);
    ui.window().dispatch_event(WindowEvent::PointerPressed { position: start, button: PointerEventButton::Left });
    ui.window().dispatch_event(WindowEvent::PointerMoved { position: end });
    ui.window().dispatch_event(WindowEvent::PointerReleased { position: end, button: PointerEventButton::Left });
    assert_eq!(plot.size().height, 300.0);
    ui.set_eq_plot_height(520.0);
    assert_eq!(plot.size().height, 520.0);
}

#[test]
fn both_plots_report_their_display_size_after_resizing() {
    let ui = window();
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(900.0, 940.0));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(10));
    let original_eq = ui.get_eq_plot_extent();
    let original_live = ui.get_live_plot_extent();
    assert_eq!(original_eq.height, 260.0);
    assert!(original_live.width > 0.0 && original_live.height > 0.0);

    ui.window().set_size(LogicalSize::new(1120.0, 940.0));
    ui.set_eq_plot_height(360.0);
    ui.set_live_view_height(300.0);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(10));
    let eq = ui.get_eq_plot_extent();
    let live = ui.get_live_plot_extent();
    assert!(eq.width > original_eq.width && live.width > original_live.width);
    assert_eq!(eq.height, 360.0);
    assert!(live.height > original_live.height);
}

#[test]
fn live_view_toggle_collapses_the_panel_and_preserves_its_settings() {
    let ui = window();
    ui.set_live_mode(3);
    ui.set_live_view_height(300.0);
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(1120.0, 1040.0));
    let live_panel = || ElementHandle::find_by_element_id(&ui, "MainWindow::live-view").next();
    let plot = ElementHandle::find_by_element_id(&ui, "MainWindow::plot").next().unwrap();
    let initial_plot_y = plot.absolute_position().y;
    let toggle = element(&ui, "Live View", AccessibleRole::Checkbox);

    toggle.invoke_accessible_default_action();
    assert!(live_panel().is_none());
    let collapsed_plot_y = plot.absolute_position().y;
    assert!(collapsed_plot_y < initial_plot_y);
    ui.set_live_view_height(420.0);
    assert_eq!(plot.absolute_position().y, collapsed_plot_y);
    ui.set_live_view_height(300.0);
    assert_eq!(ui.get_live_mode(), 3);

    toggle.invoke_accessible_default_action();
    assert_eq!(live_panel().unwrap().size().height, 300.0);
    assert_eq!(plot.absolute_position().y, initial_plot_y);
}

#[test]
fn live_view_grows_in_both_layouts() {
    let ui = window();
    ui.set_bands(Rc::new(VecModel::from(vec![BandRow::default()])).into());
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(1120.0, 1040.0));
    let panel = ElementHandle::find_by_element_id(&ui, "MainWindow::live-view").next().unwrap();
    for simple in [true, false] {
        ui.set_simple_layout(simple);
        ui.set_live_view_height(146.0);
        let compact = panel.size().height;
        ui.set_live_view_height(216.0);
        let medium = panel.size().height;
        ui.set_live_view_height(300.0);
        let large = panel.size().height;
        assert!(compact < medium && medium < large, "simple={simple}: {compact}, {medium}, {large}");
    }

}

#[test]
fn window_scroll_reaches_the_last_band() {
    let ui = window();
    ui.set_bands(Rc::new(VecModel::from(vec![BandRow::default(); 32])).into());
    ui.set_live_view_height(300.0);
    ui.set_eq_plot_height(520.0);
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(860.0, 520.0));
    let panel = ElementHandle::find_by_element_id(&ui, "MainWindow::live-view").next().unwrap();
    assert_eq!(panel.size().height, 300.0);
    assert!(ElementHandle::find_by_accessible_label(&ui, "Band 32 enabled").next().is_none());

    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position: LogicalPosition::new(10.0, 180.0),
        delta_x: 0.0,
        delta_y: -4000.0,
    });
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(250));

    assert!(panel.absolute_position().y < 0.0);
    let last_band = element(&ui, "Band 32 enabled", AccessibleRole::Checkbox);
    let band_top = last_band.absolute_position().y;
    assert!(band_top >= 0.0 && band_top + last_band.size().height <= 520.0);
}

#[test]
fn preset_naming_can_be_retried_in_the_same_popup() {
    let ui = window();
    ui.set_presets(Rc::new(VecModel::from(vec![SharedString::from("Flat")])).into());
    let requests = Rc::new(RefCell::new(Vec::new()));
    let capture = requests.clone();
    ui.on_name_preset(move |rename, name| {
        capture.borrow_mut().push((rename, name.clone()));
        if name.is_empty() { "Enter a name".into() } else { "".into() }
    });
    ui.show().unwrap();
    element(&ui, "Select Preset", AccessibleRole::Button).invoke_accessible_default_action();
    element(&ui, "Save as Preset", AccessibleRole::Button).invoke_accessible_default_action();
    element(&ui, "Save", AccessibleRole::Button).invoke_accessible_default_action();
    for character in "Late Night".chars() {
        key(&ui, character.to_string());
    }
    key(&ui, Key::Return);
    assert_eq!(*requests.borrow(), vec![(false, "".into()), (false, "Late Night".into())]);
}

#[test]
fn overwrite_preset_requires_an_editable_standard_preset() {
    let ui = window();
    ui.show().unwrap();
    let activations = Rc::new(Cell::new(0));
    let captured = activations.clone();
    ui.on_overwrite_preset(move || {
        captured.set(captured.get() + 1);
        "".into()
    });

    let overwrite = || element(&ui, "Overwrite Preset", AccessibleRole::Button);
    element(&ui, "Select Preset", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(overwrite().accessible_enabled(), Some(false));
    ui.set_can_rename(true);
    ui.set_simple_layout(true);
    assert_eq!(overwrite().accessible_enabled(), Some(false));
    ui.set_simple_layout(false);
    assert_eq!(overwrite().accessible_enabled(), Some(true));
    overwrite().invoke_accessible_default_action();
    assert_eq!(activations.get(), 1);
}

#[test]
fn standard_eq_monitor_shows_output_peak_and_clip_state() {
    let ui = window();
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(1120.0, 1000.0));
    let shows = |label| ElementHandle::find_by_accessible_label(&ui, label).next().is_some();
    assert!(shows("OFFLINE"));
    ui.set_connected(true);
    assert!(shows("NO CLIP"));
    ui.set_output_peak_dbfs(-6.0);
    assert!(shows("-6 dBFS"));
    ui.set_clipping(true);
    assert!(shows("CLIPPING"));
    ui.set_simple_layout(true);
    assert!(!shows("CLIPPING"));
}

#[test]
fn header_controls_stay_in_place_across_layouts() {
    let ui = window();
    ui.set_simple_layout(true);
    ui.set_presets(Rc::new(VecModel::from(vec![SharedString::from("Flat")])).into());
    let bypass = Rc::new(Cell::new(None));
    let captured = bypass.clone();
    ui.on_change_bypass(move |enabled| captured.set(Some(enabled)));
    let preset = Rc::new(Cell::new(None));
    let captured = preset.clone();
    ui.on_choose_preset(move |index| captured.set(Some(index)));
    ui.show().unwrap();

    let header_positions = || {
        ["Live View", "Select Preset", "Bypass"].map(|label| {
            let role = if label == "Select Preset" { AccessibleRole::Button } else { AccessibleRole::Checkbox };
            element(&ui, label, role).absolute_position()
        })
    };
    let simple_positions = header_positions();
    element(&ui, "Live View", AccessibleRole::Checkbox).invoke_accessible_default_action();
    element(&ui, "Bypass", AccessibleRole::Checkbox).invoke_accessible_default_action();
    assert_eq!(bypass.get(), Some(true));
    ui.set_simple_layout(false);
    assert_eq!(header_positions(), simple_positions);
    assert!(ui.get_bypass());
    assert!(!ui.get_show_live_view());
    ui.set_simple_layout(true);

    element(&ui, "Select Preset", AccessibleRole::Button).invoke_accessible_default_action();
    element(&ui, "Flat (selected)", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(preset.get(), Some(0));
}

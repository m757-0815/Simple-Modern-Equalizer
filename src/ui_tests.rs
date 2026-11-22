use crate::{BandRow, MainWindow, ToneClipping};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, Model, SharedString, VecModel, platform::{Key, PointerEventButton, WindowEvent}};
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

fn open_settings(ui: &MainWindow) {
    element(ui, "Settings", AccessibleRole::Button).invoke_accessible_default_action();
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
fn settings_submits_and_resets_all_colors() {
    let ui = window();
    ui.show().unwrap();
    let submitted = Rc::new(RefCell::new(Vec::new()));
    let captured = Rc::clone(&submitted);
    ui.on_change_colors(move |graph, text| {
        captured.borrow_mut().push([graph, text]);
        "".into()
    });
    open_settings(&ui);
    element(&ui, "Graphs color picker", AccessibleRole::Button).invoke_accessible_default_action();
    element(&ui, "Lavender", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(element(&ui, "Graphs color", AccessibleRole::TextInput).accessible_value().unwrap(), "#B8A0FF");
    element(&ui, "Accent text color picker", AccessibleRole::Button).invoke_accessible_default_action();
    element(&ui, "Amber", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(element(&ui, "Accent text color", AccessibleRole::TextInput).accessible_value().unwrap(), "#FFBD72");
    element(&ui, "Accent text color", AccessibleRole::TextInput).set_accessible_value("#ABCDEF");
    element(&ui, "Apply colors", AccessibleRole::Button).invoke_accessible_default_action();
    element(&ui, "Reset colors", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(submitted.borrow().as_slice(), &[
        ["#B8A0FF", "#ABCDEF"].map(SharedString::from),
        ["#77D9BD", "#77D9BD"].map(SharedString::from),
    ]);
}

#[test]
fn about_opens_in_both_layouts_and_shows_the_license() {
    let ui = window();
    let license = include_str!("../LICENSE").trim();
    ui.set_license_text(license.into());
    ui.window().set_size(LogicalSize::new(860.0, 520.0));
    ui.show().unwrap();

    for simple in [true, false] {
        ui.set_simple_layout(simple);
        open_settings(&ui);
        element(&ui, "About", AccessibleRole::Button).invoke_accessible_default_action();
        element(&ui, "Read GPLv3", AccessibleRole::Button).invoke_accessible_default_action();
        let license_view = element(&ui, "GNU General Public License", AccessibleRole::TextInput);
        assert_eq!(license_view.accessible_value().unwrap(), license);
        element(&ui, "Back to About", AccessibleRole::Button).invoke_accessible_default_action();
        element(&ui, "Close", AccessibleRole::Button).invoke_accessible_default_action();
        assert!(ElementHandle::find_by_accessible_label(&ui, "Read GPLv3").next().is_none());
    }
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
fn simple_auto_headroom_stays_in_place_when_toggled() {
    let ui = window();
    ui.set_simple_layout(true);
    let changes = Rc::new(RefCell::new(Vec::new()));
    let captured = changes.clone();
    ui.on_change_headroom(move |enabled| captured.borrow_mut().push(enabled));
    ui.show().unwrap();

    let headroom = element(&ui, "Auto Headroom", AccessibleRole::Checkbox);
    let origin = headroom.absolute_position();
    let size = headroom.size();
    let position = LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0);
    for enabled in [true, false, true] {
        click(&ui, position);
        assert_eq!(ui.get_auto_headroom(), enabled);
        assert_eq!(headroom.absolute_position(), origin);
        assert_eq!(headroom.size(), size);
    }
    assert_eq!(*changes.borrow(), [true, false, true]);
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
    plus.invoke_accessible_default_action();
    assert!((submitted.get() - 0.05).abs() < 1e-6);
    slider.invoke_accessible_increment_action();
    assert!((ui.get_preamp() - 0.05).abs() < 1e-6);
    element(&ui, "Auto Headroom", AccessibleRole::Checkbox).invoke_accessible_default_action();
    assert_eq!(slider.accessible_enabled(), Some(false));
    assert_eq!(plus.accessible_enabled(), Some(false));
    assert_eq!(entry.accessible_enabled(), Some(false));
}

#[test]
fn graph_continues_dragging_the_renumbered_band() {
    let ui = window();
    ui.set_show_live_view(false);
    ui.on_hit_band(|_, _, _, _| 0);
    let moves = Rc::new(RefCell::new(Vec::new()));
    let captured = moves.clone();
    ui.on_move_band(move |index, dx, dy| {
        captured.borrow_mut().push((index, dx, dy));
        if index == 0 { 2 } else { index }
    });
    ui.show().unwrap();
    let plot = ElementHandle::find_by_element_id(&ui, "MainWindow::plot").next().unwrap();
    let point = plot.absolute_position();
    let y = point.y + plot.size().height / 2.0;
    let start = LogicalPosition::new(point.x + 40.0, y);
    ui.window().dispatch_event(WindowEvent::PointerPressed { position: start, button: PointerEventButton::Left });
    for offset in [180.0, 260.0] {
        let position = LogicalPosition::new(start.x + offset, y);
        ui.window().dispatch_event(WindowEvent::PointerMoved { position });
    }
    let moves = moves.borrow();
    assert_eq!(moves.len(), 2);
    assert_eq!((moves[0].0, moves[1].0), (0, 2));
    assert!(moves[1].1 > moves[0].1 && moves.iter().all(|action| action.2 == 0.0));
}

#[test]
fn empty_graph_double_click_adds_one_dot_and_existing_dots_are_left_alone() {
    let ui = window();
    ui.set_show_live_view(false);
    ui.on_hit_band(|x, _, _, _| if x < 0.5 { 0 } else { -1 });
    let added = Rc::new(RefCell::new(Vec::new()));
    let captured = added.clone();
    ui.on_add_band(move |x, y, _, _| captured.borrow_mut().push((x, y)));
    ui.show().unwrap();
    let plot = ElementHandle::find_by_element_id(&ui, "MainWindow::plot").next().unwrap();
    let origin = plot.absolute_position();
    for x in [0.25, 0.75] {
        let position = LogicalPosition::new(origin.x + plot.size().width * x, origin.y + plot.size().height * 0.4);
        click(&ui, position);
        click(&ui, position);
    }
    let added = added.borrow();
    assert_eq!(added.len(), 1);
    assert!((added[0].0 - 0.75).abs() < 0.01 && (added[0].1 - 0.4).abs() < 0.01);
}

#[test]
fn edited_band_removal_can_be_cancelled_or_confirmed() {
    let ui = window();
    let decision = Rc::new(Cell::new(None));
    let captured = decision.clone();
    ui.on_resolve_band_removal(move |confirmed| captured.set(Some(confirmed)));
    ui.show().unwrap();
    for (button, confirmed) in [("Cancel", false), ("Remove", true)] {
        ui.invoke_warn_band_removal(3, 5, 1);
        element(&ui, button, AccessibleRole::Button).invoke_accessible_default_action();
        assert_eq!(decision.get(), Some(confirmed));
    }
}

#[test]
fn delete_button_targets_its_own_eq_band() {
    let ui = window();
    ui.set_show_live_view(false);
    ui.set_eq_plot_height(130.0);
    ui.set_bands(Rc::new(VecModel::from(vec![BandRow::default(); 3])).into());
    let requested = Rc::new(Cell::new(None));
    let captured = requested.clone();
    let weak = ui.as_weak();
    ui.on_delete_band(move |index| {
        captured.set(Some(index));
        weak.upgrade().unwrap().invoke_warn_single_band_removal(index + 1);
    });
    ui.show().unwrap();
    element(&ui, "Delete band 2", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(requested.get(), Some(1));
    element(&ui, "Cancel", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(ui.get_bands().row_count(), 3);
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
    let end = LogicalPosition::new(start.x, start.y + 350.0);
    ui.window().dispatch_event(WindowEvent::PointerPressed { position: start, button: PointerEventButton::Left });
    ui.window().dispatch_event(WindowEvent::PointerMoved { position: end });
    ui.window().dispatch_event(WindowEvent::PointerReleased { position: end, button: PointerEventButton::Left });
    assert_eq!(plot.size().height, 610.0);
    ui.set_eq_plot_height(1800.0);
    assert_eq!(plot.size().height, 1800.0);
}

#[test]
fn shrinking_eq_from_its_bottom_edge_keeps_its_top_fixed_while_scrolled() {
    let ui = window();
    ui.set_show_live_view(false);
    ui.set_eq_plot_height(420.0);
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(860.0, 520.0));
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position: LogicalPosition::new(10.0, 180.0),
        delta_x: 0.0,
        delta_y: -4000.0,
    });
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(250));

    let plot = ElementHandle::find_by_element_id(&ui, "MainWindow::plot").next().unwrap();
    let grip = element(&ui, "EQ graph height", AccessibleRole::Slider);
    let top = plot.absolute_position().y;
    let bottom = grip.absolute_position();
    let start = LogicalPosition::new(bottom.x + grip.size().width / 2.0, bottom.y + grip.size().height / 2.0);
    assert!((0.0..520.0).contains(&start.y));
    let end = LogicalPosition::new(start.x, start.y - 70.0);
    ui.window().dispatch_event(WindowEvent::PointerPressed { position: start, button: PointerEventButton::Left });
    ui.window().dispatch_event(WindowEvent::PointerMoved { position: end });
    ui.window().dispatch_event(WindowEvent::PointerReleased { position: end, button: PointerEventButton::Left });

    assert_eq!(plot.size().height, 350.0);
    assert_eq!(plot.absolute_position().y, top);
    assert_eq!(grip.absolute_position().y, bottom.y - 70.0);
}

#[test]
fn live_panel_selects_all_displays_and_visibility_stays_in_settings() {
    let ui = window();
    ui.set_simple_layout(true);
    ui.show().unwrap();
    let display = element(&ui, "Live display", AccessibleRole::Combobox);
    assert_eq!(display.accessible_value().unwrap(), "Analog VU");
    display.invoke_accessible_expand_action();
    for (index, label) in ["Spectrum", "Bars", "Oscilloscope", "Stereo Level"].into_iter().enumerate() {
        key(&ui, Key::DownArrow);
        assert_eq!(ui.get_live_mode(), index as i32 + 1);
        assert_eq!(display.accessible_value().unwrap(), label);
    }
    key(&ui, Key::Return);
    ui.set_simple_layout(false);
    assert_eq!(element(&ui, "Live display", AccessibleRole::Combobox).accessible_value().unwrap(), "Stereo Level");
    open_settings(&ui);
    let toggle = element(&ui, "Live View", AccessibleRole::Checkbox);
    toggle.invoke_accessible_default_action();
    assert!(!ui.get_show_live_view());
    assert!(ElementHandle::find_by_accessible_label(&ui, "Live display").next().is_none());
    toggle.invoke_accessible_default_action();
    assert_eq!(element(&ui, "Live display", AccessibleRole::Combobox).accessible_value().unwrap(), "Stereo Level");
}

#[test]
fn stereo_level_displays_separate_channel_readings_in_both_layouts() {
    let ui = window();
    ui.set_live_mode(4);
    ui.set_left_level_dbfs(-12.0);
    ui.set_right_level_dbfs(-30.0);
    ui.show().unwrap();

    let shows = |label| ElementHandle::find_by_accessible_label(&ui, label).next().is_some();
    for simple in [true, false] {
        ui.set_simple_layout(simple);
        assert!(shows("Left level: -12 dBFS"));
        assert!(shows("Right level: -30 dBFS"));
    }
    ui.set_right_level_dbfs(-1000.0);
    assert!(shows("Left level: -12 dBFS"));
    assert!(shows("Right level: −∞ dBFS"));
    ui.set_live_mode(0);
    assert!(!shows("Left level: -12 dBFS"));
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
    open_settings(&ui);
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
fn analog_vu_scales_proportionally_and_stays_centered() {
    let ui = window();
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(860.0, 900.0));
    let mut previous_width = 0.0;
    for height in [146.0, 216.0, 420.0] {
        ui.set_live_view_height(height);
        let plot = ElementHandle::find_by_element_id(&ui, "MainWindow::live-plot").next().unwrap();
        let vu = ElementHandle::find_by_element_id(&ui, "MainWindow::vu-plot").next().unwrap();
        let size = vu.size();
        assert!((size.width - 4.0 * size.height).abs() < 0.01);
        assert!(size.width > previous_width);
        assert!(size.width <= plot.size().width && size.height <= plot.size().height);
        let inset = vu.absolute_position().x - plot.absolute_position().x;
        assert!((2.0 * inset - (plot.size().width - size.width)).abs() < 0.01);
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(16));
        let extent = ui.get_live_plot_extent();
        assert_eq!((extent.width, extent.height), (size.width, size.height));
        previous_width = size.width;
    }
    ui.set_live_mode(1);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(16));
    let plot = ElementHandle::find_by_element_id(&ui, "MainWindow::live-plot").next().unwrap();
    let extent = ui.get_live_plot_extent();
    assert_eq!((extent.width, extent.height), (plot.size().width, plot.size().height));
}

#[test]
fn stereo_tick_labels_fit_between_the_meters_and_status_at_minimum_size() {
    let ui = window();
    ui.set_live_mode(4);
    ui.set_live_view_height(146.0);
    ui.set_signal("Waiting for audio engine".into());
    ui.window().set_size(LogicalSize::new(860.0, 520.0));
    ui.show().unwrap();
    for simple in [true, false] {
        ui.set_simple_layout(simple);
        let find = |id| ElementHandle::find_by_element_id(&ui, id).next().unwrap();
        let plot = find("MainWindow::live-plot");
        let scale = find("MainWindow::stereo-scale");
        let status = find("MainWindow::live-status");
        assert!(scale.absolute_position().y >= plot.absolute_position().y);
        let scale_bottom = scale.absolute_position().y + scale.size().height;
        assert!(scale_bottom <= plot.absolute_position().y + plot.size().height);
        assert!(scale_bottom < status.absolute_position().y);
    }
}

#[test]
fn spline_conversion_is_available_from_standard_settings() {
    let ui = window();
    let invoked = Rc::new(Cell::new(false));
    let capture = invoked.clone();
    ui.on_enable_spline(move || capture.set(true));
    ui.show().unwrap();
    assert!(ElementHandle::find_by_accessible_label(&ui, "Use spline").next().is_none());
    open_settings(&ui);
    element(&ui, "Use spline", AccessibleRole::Button).invoke_accessible_default_action();
    assert!(invoked.get());
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
fn simple_clipping_labels_follow_the_detected_ranges_even_with_live_view_hidden() {
    let ui = window();
    ui.set_simple_layout(true);
    ui.set_show_live_view(false);
    ui.show().unwrap();

    let shows = |label| ElementHandle::find_by_accessible_label(&ui, label).next().is_some();
    ui.set_clipping(true);
    assert!(shows("OUTPUT CLIPPING"));
    ui.set_simple_clipping(ToneClipping { bass: true, mid: false, treble: true });
    assert!(shows("Bass · CLIP"));
    assert!(shows("Treble · CLIP"));
    assert!(!shows("Mid · CLIP"));
    assert!(!shows("OUTPUT CLIPPING"));
    ui.set_show_clipping(false);
    assert!(!shows("Bass · CLIP"));
    ui.set_show_clipping(true);
    ui.set_clipping(false);
    assert!(!shows("Bass · CLIP"));
    assert!(!shows("Treble · CLIP"));
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
fn escape_exits_full_screen_and_clears_the_settings_checkbox() {
    let ui = window();
    ui.set_show_live_view(false);
    let requests = Rc::new(RefCell::new(Vec::new()));
    let captured = requests.clone();
    ui.on_change_full_screen(move |enabled| captured.borrow_mut().push(enabled));
    ui.show().unwrap();

    for focus_preamp in [false, true] {
        open_settings(&ui);
        let full_screen = element(&ui, "Full screen", AccessibleRole::Checkbox);
        assert_eq!(full_screen.accessible_checked(), Some(false));
        full_screen.invoke_accessible_default_action();
        assert!(ui.get_fullscreen_enabled());
        if focus_preamp {
            let entry = element(&ui, "Preamp in dB", AccessibleRole::TextInput);
            let point = entry.absolute_position();
            let size = entry.size();
            click(&ui, LogicalPosition::new(point.x + size.width / 2.0, point.y + size.height / 2.0));
        }
        key(&ui, Key::Escape);
        assert!(!ui.get_fullscreen_enabled());
    }
    key(&ui, Key::Escape);
    assert_eq!(&*requests.borrow(), &[true, false, true, false]);
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
        ["Simple", "Standard", "Select Preset", "Settings", "Enable Equalizer"].map(|label|
            element(&ui, label, AccessibleRole::Button).absolute_position())
    };
    let simple_positions = header_positions();
    let equalizer = element(&ui, "Enable Equalizer", AccessibleRole::Button);
    assert!(!ui.get_bypass());
    equalizer.invoke_accessible_default_action();
    open_settings(&ui);
    element(&ui, "Live View", AccessibleRole::Checkbox).invoke_accessible_default_action();
    assert_eq!(bypass.get(), Some(true));
    ui.set_simple_layout(false);
    assert_eq!(header_positions(), simple_positions);
    assert!(ui.get_bypass());
    assert!(!ui.get_show_live_view());
    for label in ["Show A/B", "Set A", "End A/B"] {
        assert!(ElementHandle::find_by_accessible_label(&ui, label).next().is_none());
    }
    ui.set_simple_layout(true);

    key(&ui, Key::Escape);
    equalizer.invoke_accessible_default_action();
    assert_eq!(bypass.get(), Some(false));
    assert!(!ui.get_bypass());
    element(&ui, "Select Preset", AccessibleRole::Button).invoke_accessible_default_action();
    element(&ui, "Flat (selected)", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(preset.get(), Some(0));
    ui.set_show_preset_button(false);
    for simple in [true, false] {
        ui.set_simple_layout(simple);
        assert_eq!(element(&ui, "Enable Equalizer", AccessibleRole::Button).accessible_enabled(), Some(true));
    }
}

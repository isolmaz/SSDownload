//! Placement of every dialog kind's controls for the current size and DPI.

use super::*;

pub(super) unsafe fn layout_dialog(dlg: &DialogUi) {
    let mut r: RECT = zeroed();
    GetClientRect(dlg.hwnd, &mut r);
    let w = r.right;
    let actual_height = r.bottom;
    let logical_height = match &dlg.kind {
        DialogKind::Wizard(f) => wizard_content_height(f.intro_lines, f.caption_lines, modern()),
        DialogKind::Add(f) => add_content_height(f.advanced),
        DialogKind::Settings(f) => {
            settings_logical_height(modern(), UiMode::parse(&f.value.ui_mode).is_simple())
        }
        DialogKind::Tools(_) => 425,
        DialogKind::Queues(_) if modern() => QUEUES_HEIGHT,
        DialogKind::Rules(_) | DialogKind::Crawler(_) | DialogKind::Synchronization(_)
            if modern() =>
        {
            FORM_HEIGHT
        }
        DialogKind::Queues(_) => 425,
        DialogKind::Rules(_) => 425,
        DialogKind::Crawler(_) => 425,
        DialogKind::Synchronization(_) => 425,
        DialogKind::Events(_) | DialogKind::JobLog(_) if modern() => FORM_HEIGHT,
        DialogKind::Events(_) => 425,
        DialogKind::JobLog(_) => 425,
        DialogKind::Speed(f) if modern() => one_field_height(f.label_lines),
        DialogKind::Rename(_) if modern() => one_field_height(1),
        DialogKind::Speed(_) => 110,
        DialogKind::Rename(_) => 110,
        DialogKind::Logins(_) if modern() => FORM_HEIGHT,
        DialogKind::Logins(_) => 425,
        DialogKind::Media(f) if modern() => media_content_height(dlg, f),
        DialogKind::Progress(_) if modern() => PROGRESS_HEIGHT,
        DialogKind::Media(f) => {
            if f.advanced {
                682
            } else {
                330
            }
        }
        DialogKind::Progress(_) => 330,
    };
    let h = actual_height.max(scale(logical_height, dlg.dpi));
    let m = scale(DIALOG_MARGIN, dlg.dpi);
    let gap = scale(DIALOG_GAP, dlg.dpi);
    let row = scale(DIALOG_ROW, dlg.dpi);
    // Every dialog reserves the same bottom button row; the error line sits
    // just above it and the body fills the rest.
    let by = h - m - row;
    move_id(
        dlg.hwnd,
        D_ERROR,
        m,
        by - scale(29, dlg.dpi),
        w - 2 * m,
        scale(20, dlg.dpi),
    );
    match &dlg.kind {
        DialogKind::Wizard(f) => {
            layout_wizard(dlg, h, f);
            if !modern() {
                MoveWindow(
                    f.start,
                    w - m - scale(110, dlg.dpi),
                    by,
                    scale(110, dlg.dpi),
                    row,
                    1,
                );
                move_id(
                    dlg.hwnd,
                    D_CANCEL,
                    w - m - scale(210, dlg.dpi),
                    by,
                    scale(92, dlg.dpi),
                    row,
                );
            }
        }
        DialogKind::Add(f) if !modern() => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let line = scale(22, dlg.dpi);
            let step = scale(26, dlg.dpi);
            let cw = w - 2 * m;
            let field = |index: usize, y: i32, show: bool| {
                if let Some(label) = labels.get(index) {
                    ShowWindow(*label, if show { SW_SHOW } else { SW_HIDE });
                    MoveWindow(*label, m, y, cw, scale(18, dlg.dpi), 1);
                }
            };
            field(0, m, true);
            MoveWindow(f.urls, m, m + scale(20, dlg.dpi), cw, scale(56, dlg.dpi), 1);
            field(1, m + scale(80, dlg.dpi), true);
            MoveWindow(
                f.dir,
                m,
                m + scale(100, dlg.dpi),
                cw - scale(96, dlg.dpi),
                line,
                1,
            );
            move_id(
                dlg.hwnd,
                D_BROWSE,
                w - m - scale(88, dlg.dpi),
                m + scale(100, dlg.dpi),
                scale(88, dlg.dpi),
                line,
            );
            field(2, m + scale(126, dlg.dpi), true);
            MoveWindow(f.filename, m, m + scale(146, dlg.dpi), cw, line, 1);
            field(3, m + scale(172, dlg.dpi), true);
            // A closed combo box only needs one row; the list it drops belongs
            // to the item count, not to the control.
            MoveWindow(
                f.kind,
                m,
                m + scale(192, dlg.dpi),
                scale(260, dlg.dpi),
                line,
                1,
            );
            let label_w = scale(276, dlg.dpi);
            for (index, (label_index, control)) in
                [(4, f.connections), (5, f.checksum), (6, f.schedule)]
                    .iter()
                    .enumerate()
            {
                let y = m + scale(222, dlg.dpi) + step * index as i32;
                if let Some(label) = labels.get(*label_index) {
                    ShowWindow(*label, if f.advanced { SW_SHOW } else { SW_HIDE });
                    MoveWindow(
                        *label,
                        m,
                        y + scale(2, dlg.dpi),
                        label_w,
                        scale(18, dlg.dpi),
                        1,
                    );
                }
                ShowWindow(*control, if f.advanced { SW_SHOW } else { SW_HIDE });
                MoveWindow(*control, m + label_w + gap, y, cw - label_w - gap, line, 1);
            }
            move_id(dlg.hwnd, D_ADVANCED, m, by, scale(185, dlg.dpi), row);
            move_id(
                dlg.hwnd,
                D_ADD_OK,
                w - m - scale(150, dlg.dpi),
                by,
                scale(150, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                D_CANCEL,
                w - m - scale(250, dlg.dpi),
                by,
                scale(92, dlg.dpi),
                row,
            );
        }
        DialogKind::Add(f) => {
            // Windows 11 dialog rhythm: a label 8 epx above every 32 epx field,
            // 12 epx between groups, 24 epx padding, the command bar below.
            let labels = child_statics(dlg.hwnd, dlg.error);
            let s = |value: i32| scale(value, dlg.dpi);
            let x = s(ADD_PAD);
            let cw = w - 2 * x;
            let label = |index: usize, lx: i32, ly: i32, lw: i32, show: bool| {
                if let Some(label) = labels.get(index) {
                    ShowWindow(*label, if show { SW_SHOW } else { SW_HIDE });
                    MoveWindow(*label, lx, ly, lw, s(ADD_LABEL), 1);
                }
            };
            let frame = |left: i32, top: i32, width: i32, height: i32| RECT {
                left,
                top,
                right: left + width,
                bottom: top + height,
            };
            let ring = s(FOCUS_MARGIN);
            let button = |id: i32, bx: i32, by: i32, bw: i32| {
                move_id(
                    dlg.hwnd,
                    id,
                    bx - ring,
                    by - ring,
                    bw + 2 * ring,
                    s(ADD_FIELD) + 2 * ring,
                );
            };
            let field_top = |label_top: i32| label_top + s(ADD_LABEL + ADD_LABEL_GAP);

            let mut y = s(ADD_TOP);
            label(0, x, y, cw, true);
            place_in_frame(f.urls, frame(x, field_top(y), cw, s(ADD_URLS)), dlg.dpi);
            y = field_top(y) + s(ADD_URLS + ADD_GROUP_GAP);

            let browse_w = add_button_width(dlg, D_BROWSE, 96);
            label(1, x, y, cw, true);
            place_in_frame(
                f.dir,
                frame(x, field_top(y), cw - browse_w - gap, s(ADD_FIELD)),
                dlg.dpi,
            );
            button(D_BROWSE, x + cw - browse_w, field_top(y), browse_w);
            y = field_top(y) + s(ADD_FIELD + ADD_GROUP_GAP);

            let kind_w = s(200).max(combo_face_width(f.kind, dlg.dpi));
            let name_w = cw - kind_w - s(ADD_GROUP_GAP);
            label(2, x, y, name_w, true);
            label(3, x + name_w + s(ADD_GROUP_GAP), y, kind_w, true);
            place_in_frame(
                f.filename,
                frame(x, field_top(y), name_w, s(ADD_FIELD)),
                dlg.dpi,
            );
            // The box reaches FOCUS_MARGIN past its face for the focus ring.
            place_combo(
                f.kind,
                inflate(
                    frame(
                        x + name_w + s(ADD_GROUP_GAP),
                        field_top(y),
                        kind_w,
                        s(ADD_FIELD),
                    ),
                    ring,
                    ring,
                ),
                dlg.dpi,
            );
            y = field_top(y) + s(ADD_FIELD + ADD_GROUP_GAP);

            for (label_index, control) in [(4, f.connections), (5, f.checksum), (6, f.schedule)] {
                label(label_index, x, y, cw, f.advanced);
                ShowWindow(control, if f.advanced { SW_SHOW } else { SW_HIDE });
                place_in_frame(control, frame(x, field_top(y), cw, s(ADD_FIELD)), dlg.dpi);
                if f.advanced {
                    y = field_top(y) + s(ADD_FIELD + ADD_GROUP_GAP);
                }
            }
            // The error line follows the last field shown.
            move_id(
                dlg.hwnd,
                D_ERROR,
                x,
                y - s(ADD_GROUP_GAP) + gap,
                cw,
                s(ADD_LABEL),
            );

            bar_start(dlg, h, &[(D_ADVANCED, advanced_width(dlg))]);
            command_bar(dlg, h, &[(D_ADD_OK, 120), (D_CANCEL, 96)]);
        }
        DialogKind::Settings(f) => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let simple = UiMode::parse(&f.value.ui_mode).is_simple();
            let genel = dlg.tab.get() == 0;
            // The advanced and network pages are only ever laid out for the
            // advanced mode: a simple dialog has no second page to put rows on.
            let advanced = dlg.tab.get() == 1 && !simple;
            let network = dlg.tab.get() == 2 && !simple;
            // The strip is wide enough for all four items; the page below it is
            // the dialog surface itself and paint_tabs owns the strip, so no
            // themed band or divider shows up next to or under the items.
            // One rhythm for every tab and one label column. Classic: 22 px
            // controls on a 26 px pitch in 12 px text, so no row ever touches
            // the next one. Modern: the Windows 11 rhythm of 32 epx controls on
            // a 40 epx pitch in 14 px Body text, above a command bar.
            let look = modern();
            let s = |value: i32| scale(value, dlg.dpi);
            // A width sized for a caption grows with the text: 14/12 in Modern.
            let hx = |value: i32| s(if look { value * 7 / 6 } else { value });
            let put = |control: HWND, show: bool, x: i32, y: i32, width: i32, height: i32| {
                if look {
                    let face = RECT {
                        left: x,
                        top: y,
                        right: x + width,
                        bottom: y + height,
                    };
                    place_modern(control, show, face, dlg.dpi);
                } else {
                    place(control, show, x, y, width, height);
                }
            };
            // Modern widens a slot to its text where a translation needs more:
            // `pad` is what the control keeps beside its text, in pixels.
            let fit = |control: HWND, base: i32, pad: i32| {
                if look {
                    base.max(text_width(control) + pad)
                } else {
                    base
                }
            };
            let cx = if look { s(ADD_PAD) } else { m + s(2) };
            if look {
                // The first label lines up with the page's text.
                move_id(
                    dlg.hwnd,
                    S_TABS,
                    cx - s(TAB_PAD),
                    s(16),
                    hx(430),
                    s(SELECTOR_TABS_HEIGHT),
                );
            } else {
                move_id(dlg.hwnd, S_TABS, m, m, s(430), s(30));
            }
            let cw = w - 2 * cx;
            let cy = if look {
                s(16 + SELECTOR_TABS_HEIGHT + 12)
            } else {
                m + s(38)
            };
            let line = s(if look { ADD_FIELD } else { 22 });
            let step = s(if look { 40 } else { 26 });
            // A row that holds only check boxes: WinUI's 32 epx box plus 4.
            let check_step = s(if look { 36 } else { 26 });
            let label_dy = s(if look { 6 } else { 3 });
            let label_h = s(if look { ADD_LABEL } else { 18 });
            let browser_h = s(if look { 44 } else { 38 });
            let label_w = hx(200);
            let field_x = cx + label_w;
            let small = hx(72);
            let half = (cw - gap) / 2;

            // Genel: every control stays a direct child of the dialog, so
            // GetDlgItem keeps answering while the other tab is selected. A
            // row the mode does not have is skipped whole - the cursor only
            // moves for a row that was laid out - so neither surface is left
            // with the empty place of a hidden control.
            let mut row_y = cy;
            // Arayüz düzeyi: which rows this page has at all. It is the first
            // row of the page it decides, and it stays reachable in both modes.
            place_static(&labels, 11, cx, row_y + label_dy, label_w, label_h, genel);
            put(f.ui_mode, genel, field_x, row_y, hx(110), line);
            // Arayüz dili shares the mode row: the label column belongs to it,
            // so this label sits in the field area beside the combo.
            place_static(
                &labels,
                12,
                field_x + hx(118),
                row_y + label_dy,
                hx(108),
                label_h,
                genel,
            );
            put(
                f.ui_language,
                genel,
                field_x + hx(230),
                row_y,
                cw - label_w - hx(230),
                line,
            );
            row_y += step;

            place_static(&labels, 0, cx, row_y + label_dy, label_w, label_h, genel);
            let browse = GetDlgItem(dlg.hwnd, S_BROWSE);
            let browse_w = fit(browse, hx(88), s(32));
            let dir_w = if look {
                cw - label_w - browse_w - hx(4)
            } else {
                cw - label_w - hx(92)
            };
            put(f.dir, genel, field_x, row_y, dir_w, line);
            put(browse, genel, cx + cw - browse_w, row_y, browse_w, line);
            row_y += step;

            // Eşzamanlı indirme and Bağlantı sayısı: the two transfer rows the
            // simple page keeps, under the folder they apply to. The advanced
            // page shows the same two fields with its own labels.
            let simple_transfer = genel && simple;
            for (i, (label, control)) in [(13usize, f.max_active), (14, f.connections)]
                .iter()
                .enumerate()
            {
                let y = row_y + step * i as i32;
                place_static(
                    &labels,
                    *label,
                    cx,
                    y + label_dy,
                    label_w,
                    label_h,
                    simple_transfer,
                );
                put(*control, simple_transfer, field_x, y, small, line);
            }
            if simple {
                row_y += step * 2;
            }

            // Kullanım amacı: part of the advanced answer only, so the simple
            // page has neither the row nor its label.
            let purpose = genel && !simple;
            place_static(&labels, 1, cx, row_y + label_dy, label_w, label_h, purpose);
            // Each box is as wide as its caption, so a longer translation moves the
            // next one along instead of running into it.
            let mut mode_x = field_x;
            for control in [f.mode_video, f.mode_file, f.mode_audio] {
                let mode_w = hx(58).max(text_width(control) + s(if look { 32 } else { 24 }));
                put(control, purpose, mode_x, row_y, mode_w, line);
                mode_x += mode_w + s(if look { 16 } else { 0 });
            }
            if !simple {
                row_y += step;
            }

            // Pano and başlangıç share a row in advanced mode. In simple mode
            // the clipboard watch is gone; in Classic the two remaining
            // checkboxes take the row - and the site-entry toggle, whose label
            // is the longest, gets the wider half of it.
            let clipboard = genel && !simple;
            put(f.clipboard, clipboard, cx, row_y, half, line);
            if clipboard {
                put(f.startup, genel, cx + half + gap, row_y, half, line);
            } else if look {
                // Modern's longer text gives each toggle a row of its own.
                put(f.site_entries, genel, cx, row_y, cw, line);
                row_y += check_step;
                put(f.startup, genel, cx, row_y, cw, line);
            } else {
                let site_w = half + hx(20);
                put(f.site_entries, genel, cx, row_y, site_w, line);
                put(
                    f.startup,
                    genel,
                    cx + site_w + gap,
                    row_y,
                    cw - site_w - gap,
                    line,
                );
            }
            row_y += check_step;
            put(f.tray, genel, cx, row_y, cw, line);
            row_y += check_step;
            put(f.notify, genel, cx, row_y, cw, line);
            row_y += check_step;
            put(f.dark, genel, cx, row_y, cw, line);
            row_y += check_step;
            // Tema: the box in the field column like the level box, the note
            // beside it.
            put(f.theme_label, genel, cx, row_y + label_dy, label_w, label_h);
            put(f.theme, genel, field_x, row_y, hx(110), line);
            put(
                f.theme_note,
                genel,
                field_x + hx(118),
                row_y + label_dy,
                cw - label_w - hx(118),
                label_h,
            );
            row_y += step;
            if !simple {
                put(f.site_entries, genel, cx, row_y, cw, line);
                row_y += check_step;
            }

            // The schedule toggle and its two clock fields share one row, and
            // all three are advanced-only.
            let schedule = genel && !simple;
            // The labels carry the "(SS:DD)" hint; Classic sizes their slots for
            // the longest language's text, Modern measures them.
            let clock_w = hx(56);
            let label_at = |index: usize| labels.get(index).copied().unwrap_or(null_mut());
            // Modern lines the row up from the measured texts; Classic keeps
            // its fixed slots.
            let schedule_w = fit(f.schedule, hx(194), check_text_inset(dlg.dpi));
            let start_label_x = if look {
                cx + schedule_w + s(12)
            } else {
                cx + hx(200)
            };
            let start_label_w = fit(label_at(2), hx(100), 0);
            let start_x = if look {
                start_label_x + start_label_w + s(8)
            } else {
                cx + hx(304)
            };
            let end_label_x = if look {
                start_x + clock_w + s(12)
            } else {
                cx + hx(366)
            };
            let end_label_w = fit(label_at(3), hx(80), 0);
            let end_x = if look {
                end_label_x + end_label_w + s(8)
            } else {
                cx + hx(450)
            };
            put(f.schedule, schedule, cx, row_y, schedule_w, line);
            place_static(
                &labels,
                2,
                start_label_x,
                row_y + label_dy,
                start_label_w,
                label_h,
                schedule,
            );
            put(f.start, schedule, start_x, row_y, clock_w, line);
            place_static(
                &labels,
                3,
                end_label_x,
                row_y + label_dy,
                end_label_w,
                label_h,
                schedule,
            );
            put(f.end, schedule, end_x, row_y, clock_w, line);
            if !simple {
                row_y += step;
            }
            put(f.update_check, genel, cx, row_y, cw, line);
            row_y += check_step;
            let check_updates = GetDlgItem(dlg.hwnd, ID_SETTINGS_CHECK_UPDATES);
            let install = GetDlgItem(dlg.hwnd, ID_SETTINGS_INSTALL_EXTENSION);
            let check_updates_w = fit(check_updates, hx(232), s(32));
            put(check_updates, genel, cx, row_y, check_updates_w, line);
            let install_x = if look {
                cx + check_updates_w + hx(8)
            } else {
                cx + hx(240)
            };
            put(
                install,
                genel,
                install_x,
                row_y,
                fit(install, hx(232), s(32)),
                line,
            );

            // Gelişmiş: the remaining transfer limits, the browser transfer
            // switch and logging. Eşzamanlı indirme and Bağlantı sayısı are laid
            // out by the page that shows them - here in advanced mode, on Genel
            // in simple mode - so this page owns their rows only when it is the
            // one that has them.
            if !simple {
                for (i, (label, control)) in [(4usize, f.max_active), (5, f.connections)]
                    .iter()
                    .enumerate()
                {
                    let y = cy + step * i as i32;
                    place_static(
                        &labels,
                        *label,
                        cx,
                        y + label_dy,
                        label_w,
                        label_h,
                        advanced,
                    );
                    // A two or three digit value needs no 120 px field.
                    put(*control, advanced, field_x, y, small, line);
                }
            } else {
                // The two fields belong to the simple rows that already hold
                // them; only their labels are this page's, and they must not
                // show up unplaced in the corner the mode has no row for.
                for (i, label) in [4usize, 5].iter().enumerate() {
                    place_static(
                        &labels,
                        *label,
                        cx,
                        cy + step * i as i32 + label_dy,
                        label_w,
                        label_h,
                        false,
                    );
                }
            }
            for (i, (label, control)) in [
                (6, f.media_fragments),
                (7, f.per_host),
                (8, f.speed),
                (9, f.retry),
            ]
            .iter()
            .enumerate()
            {
                let y = cy + step * (i as i32 + 2);
                place_static(
                    &labels,
                    *label,
                    cx,
                    y + label_dy,
                    label_w,
                    label_h,
                    advanced,
                );
                put(*control, advanced, field_x, y, small, line);
            }
            let browser_y = cy + step * 6;
            put(f.browser_transfer, advanced, cx, browser_y, cw, browser_h);
            let log_y = browser_y + browser_h + s(if look { 12 } else { 8 });
            place_static(
                &labels,
                10,
                cx,
                log_y + label_dy,
                label_w,
                label_h,
                advanced,
            );
            let log_level_w = if look {
                hx(140).max(combo_face_width(f.log_level, dlg.dpi))
            } else {
                hx(140)
            };
            put(f.log_level, advanced, field_x, log_y, log_level_w, line);
            // The host-log toggle shares the row with the level it belongs to
            // instead of pushing another row into the error line.
            let hosts_offset = if look { log_level_w + hx(12) } else { hx(152) };
            put(
                f.log_hosts,
                advanced,
                field_x + hosts_offset,
                log_y,
                cw - label_w - hosts_offset,
                line,
            );

            // Ağ: proxy rows, the scheduled limit row and three switches.
            for (i, (label, control)) in [
                (15usize, f.proxy_mode),
                (16, f.proxy_url),
                (17, f.proxy_user),
                (18, f.proxy_pass),
            ]
            .iter()
            .enumerate()
            {
                let y = cy + step * i as i32;
                place_static(&labels, *label, cx, y + label_dy, label_w, label_h, network);
                let width = match (i, look) {
                    (0, true) => hx(160).max(combo_face_width(f.proxy_mode, dlg.dpi)),
                    (0, false) => hx(160),
                    _ => cw - label_w,
                };
                put(*control, network, field_x, y, width, line);
            }
            let limit_y = cy + step * 4;
            let clock_w = hx(64);
            put(f.speed_schedule, network, cx, limit_y, hx(238), line);
            place_static(
                &labels,
                19,
                cx + hx(246),
                limit_y + label_dy,
                hx(62),
                label_h,
                network,
            );
            put(f.speed_start, network, cx + hx(310), limit_y, clock_w, line);
            place_static(
                &labels,
                20,
                cx + hx(382),
                limit_y + label_dy,
                hx(38),
                label_h,
                network,
            );
            put(f.speed_end, network, cx + hx(424), limit_y, clock_w, line);
            place_static(
                &labels,
                21,
                cx,
                limit_y + step + label_dy,
                label_w,
                label_h,
                network,
            );
            put(f.speed_kib, network, field_x, limit_y + step, small, line);
            // Davranış: switches, then the two labelled rows.
            let behaviour = dlg.tab.get() == 3 && !simple;
            for (i, control) in [
                f.keep_awake,
                f.sound,
                f.completion_card,
                f.takeover,
                f.metered,
                f.battery_saver,
                f.hotkey,
            ]
            .iter()
            .enumerate()
            {
                put(
                    *control,
                    behaviour,
                    cx,
                    cy + check_step * i as i32,
                    cw,
                    line,
                );
            }
            let choice_y = cy + check_step * 7;
            place_static(
                &labels,
                22,
                cx,
                choice_y + label_dy,
                hx(300),
                label_h,
                behaviour,
            );
            put(
                f.double_click,
                behaviour,
                cx + hx(304),
                choice_y,
                hx(170),
                line,
            );
            place_static(
                &labels,
                23,
                cx,
                choice_y + step + label_dy,
                hx(300),
                label_h,
                behaviour,
            );
            put(
                f.history,
                behaviour,
                cx + hx(304),
                choice_y + step,
                small,
                line,
            );

            if look {
                // The command bar: the secondary action at its left end, Save
                // and Cancel at its right, the error line just above it.
                let bar_y = h - s(ADD_FOOTER) + s(ADD_PAD);
                let save_w = add_button_width(dlg, S_OK, 120);
                let cancel_w = add_button_width(dlg, D_CANCEL, 96);
                let wizard_w = add_button_width(dlg, ID_SETTINGS_REOPEN_WIZARD, 120);
                put(
                    GetDlgItem(dlg.hwnd, ID_SETTINGS_REOPEN_WIZARD),
                    genel,
                    cx,
                    bar_y,
                    wizard_w,
                    line,
                );
                put(
                    GetDlgItem(dlg.hwnd, D_CANCEL),
                    true,
                    cx + cw - cancel_w,
                    bar_y,
                    cancel_w,
                    line,
                );
                put(
                    GetDlgItem(dlg.hwnd, S_OK),
                    true,
                    cx + cw - cancel_w - gap - save_w,
                    bar_y,
                    save_w,
                    line,
                );
                move_id(
                    dlg.hwnd,
                    D_ERROR,
                    cx,
                    h - s(ADD_FOOTER) - gap - label_h,
                    cw,
                    label_h,
                );
            } else {
                // The secondary action sits at the left end of the same button
                // row, so it never has to fight the purpose check boxes for the
                // purpose row.
                place(
                    GetDlgItem(dlg.hwnd, ID_SETTINGS_REOPEN_WIZARD),
                    genel,
                    cx,
                    by,
                    scale(200, dlg.dpi),
                    row,
                );
                move_id(
                    dlg.hwnd,
                    S_OK,
                    w - m - scale(100, dlg.dpi),
                    by,
                    scale(100, dlg.dpi),
                    row,
                );
                move_id(
                    dlg.hwnd,
                    D_CANCEL,
                    w - m - scale(208, dlg.dpi),
                    by,
                    scale(100, dlg.dpi),
                    row,
                );
            }
        }
        DialogKind::Tools(f) => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let line = scale(22, dlg.dpi);
            let cw = w - 2 * m;
            if let Some(&c) = labels.first() {
                MoveWindow(c, m, m, cw, scale(18, dlg.dpi), 1);
            }
            // The report box takes everything between the caption and the
            // progress bar; the bar sits above the error line, never on it.
            let progress_y = by - scale(29, dlg.dpi) - gap - scale(20, dlg.dpi);
            let status_top = m + line;
            MoveWindow(
                f.status,
                m,
                status_top,
                cw,
                (progress_y - gap - status_top).max(scale(140, dlg.dpi)),
                1,
            );
            MoveWindow(f.progress, m, progress_y, cw, scale(20, dlg.dpi), 1);
            MoveWindow(f.install, m, by, scale(115, dlg.dpi), row, 1);
            MoveWindow(
                f.update,
                m + scale(123, dlg.dpi),
                by,
                scale(130, dlg.dpi),
                row,
                1,
            );
            move_id(
                dlg.hwnd,
                T_BROWSER,
                m + scale(261, dlg.dpi),
                by,
                scale(170, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                D_CANCEL,
                w - m - scale(85, dlg.dpi),
                by,
                scale(85, dlg.dpi),
                row,
            );
        }
        DialogKind::Queues(f) if modern() => {
            let labels: Vec<_> = child_statics(dlg.hwnd, dlg.error)
                .into_iter()
                .filter(|value| *value != f.countdown)
                .collect();
            let label = |index: usize| labels.get(index).copied().unwrap_or(null_mut());
            let s = |value: i32| scale(value, dlg.dpi);
            let x = s(ADD_PAD);
            let room = w - 2 * x;
            // Program path and arguments stack beside the time windows, whose
            // field runs down both of their rows.
            let half = (room - s(ADD_GROUP_GAP)) / 2;
            let windows_x = x + half + s(ADD_GROUP_GAP);
            let windows_w = room - half - s(ADD_GROUP_GAP);
            // The two columns share one label band, so their fields start
            // level; each label sits at the band's bottom.
            let windows_label = label_height(dlg, label(4), windows_w);
            let program_label = label_height(dlg, label(6), half);
            let band = windows_label.max(program_label);
            let program = [field(label(6), f.program, Width::Flex, dlg.dpi)];
            let arguments = [field(label(7), f.arguments, Width::Flex, dlg.dpi)];
            let stack = band - program_label
                + form_row(dlg, &program, x, 0, half, 0, false)
                + form_row(dlg, &arguments, x, 0, half, 0, false);
            let status = s(ADD_LABEL + ADD_GROUP_GAP);
            let y = form_rows(
                dlg,
                h,
                &[
                    &[field(label(0), f.list, Width::Flex, dlg.dpi).fill()],
                    &[
                        field(label(1), f.name, Width::Flex, dlg.dpi),
                        button_cell(dlg, Q_CREATE),
                        button_cell(dlg, Q_RENAME),
                        button_cell(dlg, Q_DELETE),
                    ],
                    &[
                        field(label(2), f.concurrency, Width::Label(s(96)), dlg.dpi),
                        field(label(5), f.quota, Width::Label(s(160)), dlg.dpi),
                        field(label(8), f.delay, Width::Label(s(96)), dlg.dpi),
                        check_cell(dlg, f.enabled),
                    ],
                    &[
                        field(
                            label(3),
                            f.completion,
                            Width::Label(combo_face_width(f.completion, dlg.dpi)),
                            dlg.dpi,
                        ),
                        button_cell(dlg, Q_UPDATE),
                        button_cell(dlg, Q_MOVE_JOB),
                        button_cell(dlg, Q_CANCEL_COMPLETION),
                    ],
                ],
                stack + status,
            );
            let program_y = y + band - program_label;
            let top = program_y + form_row(dlg, &program, x, program_y, half, 0, true);
            form_row(dlg, &arguments, x, top, half, 0, true);
            MoveWindow(
                label(4),
                windows_x,
                y + band - windows_label,
                windows_w,
                windows_label,
                1,
            );
            let field_top = y + band + s(ADD_LABEL_GAP);
            place_modern(
                f.windows,
                true,
                RECT {
                    left: windows_x,
                    top: field_top,
                    right: windows_x + windows_w,
                    bottom: (y + stack - s(ADD_GROUP_GAP)).max(field_top + s(ADD_FIELD)),
                },
                dlg.dpi,
            );
            MoveWindow(f.countdown, x, y + stack, room, s(ADD_LABEL), 1);
            command_bar(dlg, h, &[(D_CANCEL, 96)]);
        }
        DialogKind::Rules(f) if modern() => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let label = |index: usize| labels.get(index).copied().unwrap_or(null_mut());
            form_rows(
                dlg,
                h,
                &[
                    &[field(label(0), f.list, Width::Flex, dlg.dpi).fill()],
                    &[field(label(1), f.host, Width::Flex, dlg.dpi)],
                    &[field(label(2), f.dir, Width::Flex, dlg.dpi)],
                    &[
                        field(
                            label(3),
                            f.priority,
                            Width::Label(scale(96, dlg.dpi)),
                            dlg.dpi,
                        ),
                        check_cell(dlg, f.subdomains),
                        check_cell(dlg, f.file),
                        check_cell(dlg, f.video),
                        check_cell(dlg, f.audio),
                    ],
                    &[
                        button_cell(dlg, R_NEW),
                        button_cell(dlg, R_SAVE),
                        button_cell(dlg, R_REMOVE),
                    ],
                ],
                0,
            );
            command_bar(dlg, h, &[(D_CANCEL, 96)]);
        }
        DialogKind::Crawler(f) if modern() => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let label = |index: usize| labels.get(index).copied().unwrap_or(null_mut());
            let number = Width::Label(scale(96, dlg.dpi));
            form_rows(
                dlg,
                h,
                &[
                    &[field(label(0), f.url, Width::Flex, dlg.dpi)],
                    &[
                        field(label(1), f.depth, number, dlg.dpi),
                        field(label(2), f.pages, number, dlg.dpi),
                        field(label(3), f.candidates, number, dlg.dpi),
                        button_cell(dlg, C_SCAN),
                    ],
                    &[field(label(4), f.list, Width::Flex, dlg.dpi).fill()],
                ],
                0,
            );
            command_bar(dlg, h, &[(C_ADD, 96), (D_CANCEL, 96)]);
        }
        DialogKind::Synchronization(f) if modern() => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let label = |index: usize| labels.get(index).copied().unwrap_or(null_mut());
            form_rows(
                dlg,
                h,
                &[
                    &[field(label(0), f.list, Width::Flex, dlg.dpi).fill()],
                    &[field(label(1), f.url, Width::Flex, dlg.dpi)],
                    &[field(label(2), f.dir, Width::Flex, dlg.dpi)],
                    &[
                        field(
                            label(3),
                            f.interval,
                            Width::Label(scale(96, dlg.dpi)),
                            dlg.dpi,
                        ),
                        check_cell(dlg, f.enabled),
                        check_cell(dlg, f.overwrite),
                    ],
                    &[
                        button_cell(dlg, Y_NEW),
                        button_cell(dlg, Y_SAVE),
                        button_cell(dlg, Y_REMOVE),
                        button_cell(dlg, Y_RUN),
                    ],
                ],
                0,
            );
            command_bar(dlg, h, &[(D_CANCEL, 96)]);
        }
        DialogKind::Logins(f) if modern() => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let label = |index: usize| labels.get(index).copied().unwrap_or(null_mut());
            form_rows(
                dlg,
                h,
                &[
                    &[field(label(0), f.list, Width::Flex, dlg.dpi).fill()],
                    &[
                        field(label(1), f.host, Width::Flex, dlg.dpi),
                        field(label(2), f.user, Width::Flex, dlg.dpi),
                    ],
                    &[
                        field(label(3), f.pass, Width::Flex, dlg.dpi),
                        check_cell(dlg, f.allow_http),
                    ],
                    &[button_cell(dlg, L_ADD), button_cell(dlg, L_REMOVE)],
                ],
                0,
            );
            command_bar(dlg, h, &[(L_SAVE, 96), (D_CANCEL, 96)]);
        }
        DialogKind::Queues(f) => {
            let labels: Vec<_> = child_statics(dlg.hwnd, dlg.error)
                .into_iter()
                .filter(|value| *value != f.countdown)
                .collect();
            let line = scale(22, dlg.dpi);
            let step = scale(26, dlg.dpi);
            let cw = w - 2 * m;
            let field = |index: usize, x: i32, y: i32, width: i32, height: i32| {
                if let Some(label) = labels.get(index) {
                    MoveWindow(
                        *label,
                        m + scale(x, dlg.dpi),
                        y,
                        scale(width, dlg.dpi).min(cw),
                        height,
                        1,
                    );
                }
            };
            // Six single rows and one two line row carry every queue setting;
            // the list above takes whatever is left.
            let block = step * 6 + scale(40, dlg.dpi);
            let edit_top = by - scale(29, dlg.dpi) - gap - block;
            let in_row = |i: i32| edit_top + step * i;
            let x = |value: i32| m + scale(value, dlg.dpi);
            field(0, 0, m, 660, scale(18, dlg.dpi));
            MoveWindow(
                f.list,
                m,
                m + scale(20, dlg.dpi),
                cw,
                (edit_top - gap - m - scale(20, dlg.dpi)).max(scale(90, dlg.dpi)),
                1,
            );
            // The name label follows its text; the field keeps its right edge.
            let name_w = labels
                .get(1)
                .map_or(0, |label| text_width(*label))
                .max(scale(40, dlg.dpi));
            if let Some(label) = labels.get(1) {
                MoveWindow(*label, m, in_row(0), name_w, line, 1);
            }
            MoveWindow(
                f.name,
                m + name_w + scale(4, dlg.dpi),
                in_row(0),
                cw - scale(300, dlg.dpi) - (name_w - scale(40, dlg.dpi)),
                line,
                1,
            );
            move_id(
                dlg.hwnd,
                Q_CREATE,
                x(668) - scale(88, dlg.dpi),
                in_row(0),
                scale(80, dlg.dpi),
                line,
            );
            move_id(
                dlg.hwnd,
                Q_RENAME,
                x(668) - scale(176, dlg.dpi),
                in_row(0),
                scale(80, dlg.dpi),
                line,
            );
            move_id(
                dlg.hwnd,
                Q_DELETE,
                x(668) - scale(256, dlg.dpi),
                in_row(0),
                scale(72, dlg.dpi),
                line,
            );
            field(2, 0, in_row(1), 150, line);
            MoveWindow(
                f.concurrency,
                x(154),
                in_row(1),
                scale(64, dlg.dpi),
                line,
                1,
            );
            MoveWindow(f.enabled, x(226), in_row(1), scale(130, dlg.dpi), line, 1);
            field(5, 364, in_row(1), 200, line);
            MoveWindow(f.quota, x(568), in_row(1), scale(92, dlg.dpi), line, 1);
            field(3, 0, in_row(2), 104, line);
            MoveWindow(
                f.completion,
                x(108),
                in_row(2),
                scale(190, dlg.dpi),
                line,
                1,
            );
            move_id(
                dlg.hwnd,
                Q_UPDATE,
                x(302),
                in_row(2),
                scale(110, dlg.dpi),
                line,
            );
            move_id(
                dlg.hwnd,
                Q_MOVE_JOB,
                x(416),
                in_row(2),
                scale(118, dlg.dpi),
                line,
            );
            move_id(
                dlg.hwnd,
                Q_CANCEL_COMPLETION,
                x(538),
                in_row(2),
                scale(122, dlg.dpi),
                line,
            );
            field(6, 0, in_row(3), 280, line);
            MoveWindow(
                f.program,
                x(284),
                in_row(3),
                cw - scale(284, dlg.dpi),
                line,
                1,
            );
            field(7, 0, in_row(4), 300, line);
            MoveWindow(
                f.arguments,
                x(304),
                in_row(4),
                cw - scale(304, dlg.dpi),
                line,
                1,
            );
            field(8, 0, in_row(5), 230, line);
            MoveWindow(f.delay, x(234), in_row(5), scale(64, dlg.dpi), line, 1);
            MoveWindow(
                f.countdown,
                x(306),
                in_row(5),
                cw - scale(306, dlg.dpi),
                line,
                1,
            );
            field(4, 0, in_row(6), 330, scale(40, dlg.dpi));
            MoveWindow(
                f.windows,
                x(334),
                in_row(6),
                cw - scale(334, dlg.dpi),
                scale(40, dlg.dpi),
                1,
            );
            move_id(
                dlg.hwnd,
                D_CANCEL,
                w - m - scale(90, dlg.dpi),
                by,
                scale(90, dlg.dpi),
                row,
            );
        }
        DialogKind::Rules(f) => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let line = scale(22, dlg.dpi);
            let step = scale(26, dlg.dpi);
            let cw = w - 2 * m;
            let field = |index: usize, x: i32, y: i32, width: i32| {
                if let Some(label) = labels.get(index) {
                    MoveWindow(
                        *label,
                        m + scale(x, dlg.dpi),
                        y,
                        scale(width, dlg.dpi).min(cw),
                        line,
                        1,
                    );
                }
            };
            // Three label/field rows, one check box row and one button row;
            // the rule list above them keeps the rest.
            let block = step * 4;
            let edit_top = by - scale(29, dlg.dpi) - gap - block;
            let in_row = |i: i32| edit_top + step * i;
            let x = |value: i32| m + scale(value, dlg.dpi);
            if let Some(label) = labels.first() {
                MoveWindow(*label, m, m, cw, scale(18, dlg.dpi), 1);
            }
            MoveWindow(
                f.list,
                m,
                m + scale(20, dlg.dpi),
                cw,
                (edit_top - gap - m - scale(20, dlg.dpi)).max(scale(90, dlg.dpi)),
                1,
            );
            field(1, 0, in_row(0), 100);
            MoveWindow(f.host, x(104), in_row(0), cw - scale(104, dlg.dpi), line, 1);
            field(2, 0, in_row(1), 120);
            MoveWindow(f.dir, x(124), in_row(1), cw - scale(124, dlg.dpi), line, 1);
            field(3, 0, in_row(2), 90);
            MoveWindow(f.priority, x(94), in_row(2), scale(72, dlg.dpi), line, 1);
            MoveWindow(
                f.subdomains,
                x(174),
                in_row(2),
                scale(180, dlg.dpi),
                line,
                1,
            );
            MoveWindow(f.file, x(362), in_row(2), scale(90, dlg.dpi), line, 1);
            MoveWindow(f.video, x(460), in_row(2), scale(94, dlg.dpi), line, 1);
            MoveWindow(f.audio, x(562), in_row(2), scale(92, dlg.dpi), line, 1);
            move_id(dlg.hwnd, R_SAVE, x(0), in_row(3), scale(140, dlg.dpi), line);
            move_id(
                dlg.hwnd,
                R_REMOVE,
                x(150),
                in_row(3),
                scale(140, dlg.dpi),
                line,
            );
            move_id(
                dlg.hwnd,
                R_NEW,
                x(300),
                in_row(3),
                scale(120, dlg.dpi),
                line,
            );
            move_id(
                dlg.hwnd,
                D_CANCEL,
                w - m - scale(90, dlg.dpi),
                by,
                scale(90, dlg.dpi),
                row,
            );
        }
        DialogKind::Crawler(f) => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let line = scale(22, dlg.dpi);
            let cw = w - 2 * m;
            let field = |index: usize, x: i32, y: i32, width: i32| {
                if let Some(label) = labels.get(index) {
                    MoveWindow(
                        *label,
                        m + scale(x, dlg.dpi),
                        y,
                        scale(width, dlg.dpi).min(cw),
                        line,
                        1,
                    );
                }
            };
            let x = |value: i32| m + scale(value, dlg.dpi);
            if let Some(label) = labels.first() {
                MoveWindow(*label, m, m, cw, scale(18, dlg.dpi), 1);
            }
            let controls_y = m + scale(20, dlg.dpi);
            MoveWindow(f.url, m, controls_y, cw, line, 1);
            let numbers_y = controls_y + line + gap;
            field(1, 0, numbers_y, 70);
            MoveWindow(f.depth, x(74), numbers_y, scale(64, dlg.dpi), line, 1);
            field(2, 150, numbers_y, 140);
            MoveWindow(f.pages, x(294), numbers_y, scale(64, dlg.dpi), line, 1);
            field(3, 370, numbers_y, 150);
            MoveWindow(
                f.candidates,
                x(524),
                numbers_y,
                scale(136, dlg.dpi),
                line,
                1,
            );
            let list_top = numbers_y + line + gap;
            MoveWindow(
                f.list,
                m,
                list_top,
                cw,
                (by - scale(29, dlg.dpi) - gap - list_top).max(scale(120, dlg.dpi)),
                1,
            );
            move_id(dlg.hwnd, C_ADD, m, by, scale(210, dlg.dpi), row);
            move_id(
                dlg.hwnd,
                C_SCAN,
                m + scale(220, dlg.dpi),
                by,
                scale(150, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                D_CANCEL,
                w - m - scale(90, dlg.dpi),
                by,
                scale(90, dlg.dpi),
                row,
            );
        }
        DialogKind::Synchronization(f) => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let line = scale(22, dlg.dpi);
            let step = scale(26, dlg.dpi);
            let cw = w - 2 * m;
            let field = |index: usize, x: i32, y: i32, width: i32| {
                if let Some(label) = labels.get(index) {
                    MoveWindow(
                        *label,
                        m + scale(x, dlg.dpi),
                        y,
                        scale(width, dlg.dpi).min(cw),
                        line,
                        1,
                    );
                }
            };
            let block = step * 4;
            let edit_top = by - scale(29, dlg.dpi) - gap - block;
            let in_row = |i: i32| edit_top + step * i;
            let x = |value: i32| m + scale(value, dlg.dpi);
            if let Some(label) = labels.first() {
                MoveWindow(*label, m, m, cw, scale(18, dlg.dpi), 1);
            }
            MoveWindow(
                f.list,
                m,
                m + scale(20, dlg.dpi),
                cw,
                (edit_top - gap - m - scale(20, dlg.dpi)).max(scale(90, dlg.dpi)),
                1,
            );
            field(1, 0, in_row(0), 60);
            MoveWindow(f.url, x(64), in_row(0), cw - scale(64, dlg.dpi), line, 1);
            field(2, 0, in_row(1), 110);
            MoveWindow(f.dir, x(114), in_row(1), cw - scale(114, dlg.dpi), line, 1);
            field(3, 0, in_row(2), 130);
            MoveWindow(f.interval, x(134), in_row(2), scale(80, dlg.dpi), line, 1);
            MoveWindow(f.enabled, x(230), in_row(2), scale(146, dlg.dpi), line, 1);
            MoveWindow(f.overwrite, x(380), in_row(2), scale(280, dlg.dpi), line, 1);
            move_id(dlg.hwnd, Y_NEW, x(0), in_row(3), scale(120, dlg.dpi), line);
            move_id(
                dlg.hwnd,
                Y_SAVE,
                x(130),
                in_row(3),
                scale(130, dlg.dpi),
                line,
            );
            move_id(
                dlg.hwnd,
                Y_REMOVE,
                x(270),
                in_row(3),
                scale(130, dlg.dpi),
                line,
            );
            move_id(
                dlg.hwnd,
                Y_RUN,
                x(410),
                in_row(3),
                scale(130, dlg.dpi),
                line,
            );
            move_id(
                dlg.hwnd,
                D_CANCEL,
                w - m - scale(90, dlg.dpi),
                by,
                scale(90, dlg.dpi),
                row,
            );
        }
        DialogKind::Events(f) if modern() => {
            layout_list_dialog(dlg, h, f.list, &[E_OPEN, E_PACKAGE], E_CLOSE);
        }
        DialogKind::JobLog(f) if modern() => {
            layout_list_dialog(dlg, h, f.list, &[J_COPY, J_OPEN], J_CLOSE);
        }
        DialogKind::Events(f) => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            if let Some(&c) = labels.first() {
                MoveWindow(c, m, m, w - 2 * m, scale(20, dlg.dpi), 1);
            }
            let list_top = m + scale(24, dlg.dpi);
            MoveWindow(
                f.list,
                m,
                list_top,
                w - 2 * m,
                (by - scale(29, dlg.dpi) - gap - list_top).max(scale(120, dlg.dpi)),
                1,
            );
            move_id(dlg.hwnd, E_OPEN, m, by, scale(160, dlg.dpi), row);
            move_id(
                dlg.hwnd,
                E_PACKAGE,
                m + scale(168, dlg.dpi),
                by,
                scale(180, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                E_CLOSE,
                w - m - scale(90, dlg.dpi),
                by,
                scale(90, dlg.dpi),
                row,
            );
        }
        DialogKind::Speed(f) if modern() => {
            layout_one_field(dlg, h, f.edit, SP_OK, f.label_lines, Some(140));
        }
        DialogKind::Rename(f) if modern() => {
            layout_one_field(dlg, h, f.edit, RN_OK, 1, None);
        }
        DialogKind::Speed(f) => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            if let Some(&c) = labels.first() {
                MoveWindow(c, m, m, w - 2 * m, scale(34, dlg.dpi), 1);
            }
            MoveWindow(
                f.edit,
                m,
                m + scale(38, dlg.dpi),
                scale(120, dlg.dpi),
                scale(22, dlg.dpi),
                1,
            );
            move_id(
                dlg.hwnd,
                SP_OK,
                w - m - scale(100, dlg.dpi),
                by,
                scale(100, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                D_CANCEL,
                w - m - scale(208, dlg.dpi),
                by,
                scale(100, dlg.dpi),
                row,
            );
        }
        DialogKind::Rename(f) => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            if let Some(&c) = labels.first() {
                MoveWindow(c, m, m, w - 2 * m, scale(20, dlg.dpi), 1);
            }
            MoveWindow(
                f.edit,
                m,
                m + scale(26, dlg.dpi),
                w - 2 * m,
                scale(22, dlg.dpi),
                1,
            );
            move_id(
                dlg.hwnd,
                RN_OK,
                w - m - scale(130, dlg.dpi),
                by,
                scale(130, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                D_CANCEL,
                w - m - scale(238, dlg.dpi),
                by,
                scale(100, dlg.dpi),
                row,
            );
        }
        DialogKind::Logins(f) => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            let line = scale(22, dlg.dpi);
            let step = scale(28, dlg.dpi);
            let label_w = scale(170, dlg.dpi);
            if let Some(&c) = labels.first() {
                MoveWindow(c, m, m, w - 2 * m, scale(20, dlg.dpi), 1);
            }
            let form_top = by - scale(29, dlg.dpi) - gap - step * 4;
            let list_top = m + scale(24, dlg.dpi);
            MoveWindow(
                f.list,
                m,
                list_top,
                w - 2 * m,
                (form_top - gap - list_top).max(scale(80, dlg.dpi)),
                1,
            );
            for (i, (label, edit)) in [(1usize, f.host), (2, f.user), (3, f.pass)]
                .iter()
                .enumerate()
            {
                let y = form_top + step * i as i32;
                if let Some(&c) = labels.get(*label) {
                    MoveWindow(c, m, y + scale(3, dlg.dpi), label_w, scale(18, dlg.dpi), 1);
                }
                MoveWindow(*edit, m + label_w, y, w - 2 * m - label_w, line, 1);
            }
            MoveWindow(
                f.allow_http,
                m + label_w,
                form_top + step * 3,
                w - 2 * m - label_w,
                line,
                1,
            );
            move_id(dlg.hwnd, L_ADD, m, by, scale(130, dlg.dpi), row);
            move_id(
                dlg.hwnd,
                L_REMOVE,
                m + scale(138, dlg.dpi),
                by,
                scale(100, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                L_SAVE,
                w - m - scale(100, dlg.dpi),
                by,
                scale(100, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                D_CANCEL,
                w - m - scale(208, dlg.dpi),
                by,
                scale(100, dlg.dpi),
                row,
            );
        }
        DialogKind::JobLog(f) => {
            let labels = child_statics(dlg.hwnd, dlg.error);
            if let Some(&c) = labels.first() {
                MoveWindow(c, m, m, w - 2 * m, scale(20, dlg.dpi), 1);
            }
            let list_top = m + scale(24, dlg.dpi);
            MoveWindow(
                f.list,
                m,
                list_top,
                w - 2 * m,
                (by - scale(29, dlg.dpi) - gap - list_top).max(scale(120, dlg.dpi)),
                1,
            );
            move_id(dlg.hwnd, J_COPY, m, by, scale(110, dlg.dpi), row);
            move_id(
                dlg.hwnd,
                J_OPEN,
                m + scale(118, dlg.dpi),
                by,
                scale(160, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                J_CLOSE,
                w - m - scale(90, dlg.dpi),
                by,
                scale(90, dlg.dpi),
                row,
            );
        }
        DialogKind::Media(f) if modern() => {
            let s = |value: i32| scale(value, dlg.dpi);
            let x = s(ADD_PAD);
            let room = w - 2 * x;
            let audio_only = f
                .kind_values
                .get(combo_index(f.kind).max(0) as usize)
                .map(|(kind, _)| *kind)
                == Some(DownloadKind::Audio);
            // A loaded thumbnail takes the right end of the summary row.
            let thumb_w = if IsWindowVisible(f.thumbnail) != 0 {
                s(100) + s(ADD_GROUP_GAP)
            } else {
                0
            };
            let summary_h = s(MEDIA_SUMMARY);
            MoveWindow(f.summary, x, s(ADD_TOP), room - thumb_w, summary_h, 1);
            MoveWindow(
                f.thumbnail,
                x + room - s(100),
                s(ADD_TOP),
                s(100),
                summary_h,
                1,
            );
            let shown = |open: bool| if open { SW_SHOW } else { SW_HIDE };
            for &label in media_labels(dlg, f).iter().skip(3) {
                ShowWindow(label, shown(f.advanced));
            }
            for control in [
                f.audio,
                f.audio_tracks,
                f.subtitles,
                f.external_url,
                f.external_language,
                f.external_label,
                f.external_kind,
                f.external_default,
                f.external_list,
                f.full_verification,
                f.playlist,
                f.playlist_items,
                f.renew_job,
                f.renew_restart,
                GetDlgItem(dlg.hwnd, ID_MEDIA_EXTERNAL_ADD),
                GetDlgItem(dlg.hwnd, ID_MEDIA_EXTERNAL_REMOVE),
                GetDlgItem(dlg.hwnd, M_RENEW),
            ] {
                ShowWindow(control, shown(f.advanced));
            }
            ShowWindow(f.from_start, shown(f.from_start_offered));
            let rows = media_rows(dlg, f);
            let rows: Vec<&[Cell]> = rows.iter().map(Vec::as_slice).collect();
            form_rows_from(dlg, h, media_rows_top(dlg.dpi), &rows, 0);
            EnableWindow(f.format, (!audio_only) as BOOL);
            EnableWindow(f.audio, audio_only as BOOL);
            EnableWindow(f.audio_tracks, 1);
            for control in [
                f.subtitles,
                f.external_url,
                f.external_language,
                f.external_label,
                f.external_kind,
                f.external_default,
                f.external_list,
                GetDlgItem(dlg.hwnd, ID_MEDIA_EXTERNAL_ADD),
                GetDlgItem(dlg.hwnd, ID_MEDIA_EXTERNAL_REMOVE),
            ] {
                EnableWindow(control, (!audio_only) as BOOL);
            }
            // The report action and "Download as MP3" take the same place
            // after Advanced options: only a failed analysis has anything to
            // report.
            ShowWindow(GetDlgItem(dlg.hwnd, M_REPORT), shown(f.report_shown));
            ShowWindow(GetDlgItem(dlg.hwnd, M_MP3), shown(!f.report_shown));
            bar_start(
                dlg,
                h,
                &[
                    (D_ADVANCED, advanced_width(dlg)),
                    (M_REPORT, add_button_width(dlg, M_REPORT, 96)),
                    (M_MP3, add_button_width(dlg, M_MP3, 96)),
                ],
            );
            command_bar(dlg, h, &[(M_QUEUE, 120), (D_CANCEL, 96)]);
        }
        DialogKind::Progress(f) if modern() => {
            let s = |value: i32| scale(value, dlg.dpi);
            let x = s(ADD_PAD);
            let room = w - 2 * x;
            // A playlist lists its jobs above the detail; a single job gives
            // that place to the status text.
            let multi = f.ids.len() > 1;
            ShowWindow(f.list, multi as BOOL);
            let line = |control: HWND, height: i32| Cell {
                label: null_mut(),
                control,
                width: Width::Flex,
                height,
            };
            let mut rows: Vec<Vec<Cell>> = vec![
                vec![
                    // Two lines: a long name or status wraps beside the buttons.
                    line(f.summary, s(2 * ADD_LABEL)),
                    check_cell(dlg, GetDlgItem(dlg.hwnd, P_PIN)),
                    button_cell(dlg, P_MINI),
                ],
                vec![line(f.hint, s(ADD_LABEL))],
            ];
            if multi {
                rows.push(vec![line(f.list, list_height(4, dlg.dpi))]);
            }
            rows.push(vec![line(f.status, 0)]);
            rows.push(vec![line(f.progress, s(16))]);
            let rows: Vec<&[Cell]> = rows.iter().map(Vec::as_slice).collect();
            // The error line shares its row with the report action it belongs
            // to, centred on the button, with the card's gap below them.
            let y = form_rows(dlg, h, &rows, s(ADD_FIELD + ADD_GROUP_GAP - ADD_LABEL));
            let report_w = add_button_width(dlg, P_REPORT, 96);
            move_id(
                dlg.hwnd,
                D_ERROR,
                x,
                y + s(ADD_FIELD - ADD_LABEL) / 2,
                room - report_w - s(DIALOG_GAP),
                s(ADD_LABEL),
            );
            let ring = s(FOCUS_MARGIN);
            move_id(
                dlg.hwnd,
                P_REPORT,
                x + room - report_w - ring,
                y - ring,
                report_w + 2 * ring,
                s(ADD_FIELD) + 2 * ring,
            );
            bar_start(
                dlg,
                h,
                &[
                    (P_ACTION, add_button_width(dlg, P_ACTION, 96)),
                    (P_CANCEL, add_button_width(dlg, P_CANCEL, 96)),
                ],
            );
            command_bar(
                dlg,
                h,
                &[(P_OPEN_FILE, 96), (P_OPEN_FOLDER, 96), (P_CLOSE, 96)],
            );
        }
        DialogKind::Media(f) => {
            // The thumbnail and the total size are STATICs too, but they are placed on
            // their own below, not as field labels.
            let labels: Vec<_> = child_statics(dlg.hwnd, dlg.error)
                .into_iter()
                .filter(|value| *value != f.thumbnail && *value != f.size)
                .collect();
            let media_kind = f
                .kind_values
                .get(combo_index(f.kind).max(0) as usize)
                .map(|(kind, _)| *kind)
                .unwrap_or(DownloadKind::Video);
            let audio_only = media_kind == DownloadKind::Audio;
            let line = scale(22, dlg.dpi);
            let cw = w - 2 * m;
            let half = (cw - gap) / 2;
            let advanced_top = m + scale(300, dlg.dpi);
            let field = |index: usize, x: i32, y: i32, width: i32| {
                if let Some(label) = labels.get(index) {
                    ShowWindow(
                        *label,
                        if index < 3 || f.advanced {
                            SW_SHOW
                        } else {
                            SW_HIDE
                        },
                    );
                    MoveWindow(*label, x, y, width.min(w - m - x), scale(18, dlg.dpi), 1);
                }
            };
            // A loaded thumbnail takes the right end of the summary row.
            let thumb_w = if IsWindowVisible(f.thumbnail) != 0 {
                scale(100, dlg.dpi) + gap
            } else {
                0
            };
            MoveWindow(f.summary, m, m, cw - thumb_w, scale(56, dlg.dpi), 1);
            MoveWindow(
                f.thumbnail,
                w - m - scale(100, dlg.dpi),
                m,
                scale(100, dlg.dpi),
                scale(56, dlg.dpi),
                1,
            );
            // Name row first: the saved name, then the output container.
            field(0, m, m + scale(62, dlg.dpi), cw);
            MoveWindow(f.name, m, m + scale(82, dlg.dpi), cw, line, 1);
            field(1, m, m + scale(108, dlg.dpi), cw);
            MoveWindow(f.kind, m, m + scale(128, dlg.dpi), cw, line, 1);
            field(2, m, m + scale(156, dlg.dpi), cw);
            // The format list owns everything between the picker rows and the
            // advanced block (or the button row when advanced is collapsed).
            let format_top = m + scale(176, dlg.dpi);
            let from_start_row = if f.from_start_offered { line + gap } else { 0 };
            let format_bottom = if f.advanced {
                advanced_top - gap
            } else {
                by - scale(29, dlg.dpi) - gap
            } - line
                - gap
                - from_start_row;
            // The remembered-choice switch sits right under the format list, and a live
            // stream's beginning switch under that.
            MoveWindow(f.remember, m, format_bottom + gap, cw, line, 1);
            MoveWindow(f.from_start, m, format_bottom + 2 * gap + line, cw, line, 1);
            ShowWindow(
                f.from_start,
                if f.from_start_offered {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
            MoveWindow(
                f.format,
                m,
                format_top,
                cw,
                (format_bottom - format_top).max(scale(120, dlg.dpi)),
                1,
            );
            EnableWindow(f.format, (!audio_only) as BOOL);
            // The selected streams' total size, right under the quality list.
            MoveWindow(
                f.size,
                m,
                format_top + line + scale(4, dlg.dpi),
                cw,
                scale(18, dlg.dpi),
                1,
            );
            // Advanced: track lists share one row and the external fields are
            // laid out for a 640 px window.
            field(3, m, advanced_top, cw);
            ShowWindow(f.audio, if f.advanced { SW_SHOW } else { SW_HIDE });
            MoveWindow(f.audio, m, advanced_top + line, cw, line, 1);
            let lists_y = advanced_top + scale(50, dlg.dpi);
            field(4, m, lists_y, half);
            field(5, m + half + gap, lists_y, half);
            MoveWindow(
                f.audio_tracks,
                m,
                lists_y + line,
                half,
                scale(56, dlg.dpi),
                1,
            );
            MoveWindow(
                f.subtitles,
                m + half + gap,
                lists_y + line,
                half,
                scale(56, dlg.dpi),
                1,
            );
            let external_y = lists_y + line + scale(64, dlg.dpi);
            field(6, m, external_y, cw);
            for control in [
                f.audio,
                f.audio_tracks,
                f.subtitles,
                f.external_url,
                f.external_language,
                f.external_label,
                f.external_kind,
                f.external_default,
                f.external_list,
                f.full_verification,
                f.playlist,
                GetDlgItem(dlg.hwnd, ID_MEDIA_EXTERNAL_ADD),
                GetDlgItem(dlg.hwnd, ID_MEDIA_EXTERNAL_REMOVE),
            ] {
                ShowWindow(control, if f.advanced { SW_SHOW } else { SW_HIDE });
            }
            MoveWindow(
                f.external_url,
                m,
                external_y + scale(20, dlg.dpi),
                cw,
                line,
                1,
            );
            let external_labels_y = external_y + scale(42, dlg.dpi);
            field(7, m, external_labels_y, scale(96, dlg.dpi));
            field(
                8,
                m + scale(104, dlg.dpi),
                external_labels_y,
                scale(140, dlg.dpi),
            );
            let external_row = external_labels_y + scale(20, dlg.dpi);
            MoveWindow(
                f.external_language,
                m,
                external_row,
                scale(96, dlg.dpi),
                line,
                1,
            );
            MoveWindow(
                f.external_label,
                m + scale(104, dlg.dpi),
                external_row,
                scale(140, dlg.dpi),
                line,
                1,
            );
            MoveWindow(
                f.external_kind,
                m + scale(252, dlg.dpi),
                external_row,
                scale(84, dlg.dpi),
                line,
                1,
            );
            MoveWindow(
                f.external_default,
                m + scale(344, dlg.dpi),
                external_row,
                scale(96, dlg.dpi),
                line,
                1,
            );
            move_id(
                dlg.hwnd,
                ID_MEDIA_EXTERNAL_ADD,
                w - m - scale(130, dlg.dpi),
                external_row,
                scale(130, dlg.dpi),
                line,
            );
            let list_y = external_row + line + gap;
            MoveWindow(
                f.external_list,
                m,
                list_y,
                cw - scale(140, dlg.dpi),
                scale(52, dlg.dpi),
                1,
            );
            move_id(
                dlg.hwnd,
                ID_MEDIA_EXTERNAL_REMOVE,
                w - m - scale(130, dlg.dpi),
                list_y,
                scale(130, dlg.dpi),
                line,
            );
            // The two checkboxes share one row so the renewal group fits the
            // recorded 640x730 advanced geometry without a scrollbar.
            MoveWindow(
                f.full_verification,
                m,
                list_y + scale(52, dlg.dpi) + gap,
                half,
                line,
                1,
            );
            MoveWindow(
                f.playlist,
                m + half + gap,
                list_y + scale(52, dlg.dpi) + gap,
                half - scale(118, dlg.dpi),
                line,
                1,
            );
            place(
                f.playlist_items,
                f.advanced,
                w - m - scale(112, dlg.dpi),
                list_y + scale(52, dlg.dpi) + gap,
                scale(112, dlg.dpi),
                line,
            );
            let renew_y = list_y + scale(52, dlg.dpi) + gap + line;
            ShowWindow(f.renew_job, if f.advanced { SW_SHOW } else { SW_HIDE });
            ShowWindow(f.renew_restart, if f.advanced { SW_SHOW } else { SW_HIDE });
            MoveWindow(f.renew_job, m, renew_y, scale(214, dlg.dpi), line, 1);
            MoveWindow(
                f.renew_restart,
                m + scale(222, dlg.dpi),
                renew_y,
                scale(190, dlg.dpi),
                line,
                1,
            );
            move_id(
                dlg.hwnd,
                M_RENEW,
                w - m - scale(180, dlg.dpi),
                renew_y,
                scale(180, dlg.dpi),
                line,
            );
            ShowWindow(
                GetDlgItem(dlg.hwnd, M_RENEW),
                if f.advanced { SW_SHOW } else { SW_HIDE },
            );
            EnableWindow(f.audio, audio_only as BOOL);
            EnableWindow(f.audio_tracks, 1);
            for control in [
                f.subtitles,
                f.external_url,
                f.external_language,
                f.external_label,
                f.external_kind,
                f.external_default,
                f.external_list,
                GetDlgItem(dlg.hwnd, ID_MEDIA_EXTERNAL_ADD),
                GetDlgItem(dlg.hwnd, ID_MEDIA_EXTERNAL_REMOVE),
            ] {
                EnableWindow(control, (!audio_only) as BOOL);
            }
            move_id(
                dlg.hwnd,
                M_QUEUE,
                w - m - scale(120, dlg.dpi),
                by,
                scale(120, dlg.dpi),
                row,
            );
            move_id(dlg.hwnd, D_ADVANCED, m, by, scale(185, dlg.dpi), row);
            // The report action takes the free middle of the row, which only
            // exists while the picker is showing a failed analysis.
            place(
                GetDlgItem(dlg.hwnd, M_REPORT),
                f.report_shown,
                m + scale(197, dlg.dpi),
                by,
                scale(112, dlg.dpi),
                row,
            );
            // "MP3 olarak indir" uses the same middle place while nothing failed.
            place(
                GetDlgItem(dlg.hwnd, M_MP3),
                !f.report_shown,
                m + scale(197, dlg.dpi),
                by,
                scale(150, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                D_CANCEL,
                w - m - scale(230, dlg.dpi),
                by,
                scale(92, dlg.dpi),
                row,
            );
        }
        DialogKind::Progress(f) => {
            // The report action shares the error line it belongs to instead of
            // taking a place in the transfer row: the line gives up exactly the
            // width the button needs, and the button keeps its own visibility.
            let report_w = scale(104, dlg.dpi);
            move_id(
                dlg.hwnd,
                D_ERROR,
                m,
                by - scale(29, dlg.dpi),
                w - 2 * m - report_w - gap,
                scale(20, dlg.dpi),
            );
            move_id(
                dlg.hwnd,
                P_REPORT,
                w - m - report_w,
                by - scale(30, dlg.dpi),
                report_w,
                scale(22, dlg.dpi),
            );
            // A playlist fits its rows above the detail; a single job has
            // nothing to choose between, so the rows give their place to the
            // status text.
            let multi = f.ids.len() > 1;
            ShowWindow(f.list, multi as BOOL);
            // "Küçült" and "Üstte tut" share the summary row, at its right end.
            let tools_w = scale(84, dlg.dpi) + gap + scale(96, dlg.dpi);
            MoveWindow(
                f.summary,
                m,
                m,
                w - 2 * m - tools_w - gap,
                scale(20, dlg.dpi),
                1,
            );
            move_id(
                dlg.hwnd,
                P_MINI,
                w - m - tools_w,
                m - scale(2, dlg.dpi),
                scale(84, dlg.dpi),
                scale(24, dlg.dpi),
            );
            move_id(
                dlg.hwnd,
                P_PIN,
                w - m - scale(96, dlg.dpi),
                m,
                scale(96, dlg.dpi),
                scale(20, dlg.dpi),
            );
            MoveWindow(
                f.hint,
                m,
                m + scale(22, dlg.dpi),
                w - 2 * m,
                scale(18, dlg.dpi),
                1,
            );
            let mut top = m + scale(46, dlg.dpi);
            if multi {
                MoveWindow(f.list, m, top, w - 2 * m, scale(92, dlg.dpi), 1);
                top += scale(92, dlg.dpi) + gap;
            }
            // The framework's error line sits just above the button row; the
            // bar takes the slot under the status text.
            let body_bottom = by - scale(29, dlg.dpi) - gap;
            let bar_y = body_bottom - scale(14, dlg.dpi);
            MoveWindow(
                f.status,
                m,
                top,
                w - 2 * m,
                (bar_y - gap - top).max(scale(60, dlg.dpi)),
                1,
            );
            MoveWindow(f.progress, m, bar_y, w - 2 * m, scale(14, dlg.dpi), 1);
            // Control pair on the left, the output actions on the right.
            move_id(dlg.hwnd, P_ACTION, m, by, scale(104, dlg.dpi), row);
            move_id(
                dlg.hwnd,
                P_CANCEL,
                m + scale(112, dlg.dpi),
                by,
                scale(104, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                P_CLOSE,
                w - m - scale(84, dlg.dpi),
                by,
                scale(84, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                P_OPEN_FOLDER,
                w - m - scale(84, dlg.dpi) - gap - scale(132, dlg.dpi),
                by,
                scale(132, dlg.dpi),
                row,
            );
            move_id(
                dlg.hwnd,
                P_OPEN_FILE,
                w - m - scale(84, dlg.dpi) - gap - scale(132, dlg.dpi) - gap - scale(108, dlg.dpi),
                by,
                scale(108, dlg.dpi),
                row,
            );
        }
    }
    if follows_reading_order(&dlg.kind, modern()) {
        tab_in_reading_order(dlg.hwnd);
    }
    let offset = dlg.scroll_y.get().clamp(0, (h - actual_height).max(0));
    dlg.scroll_y.set(offset);
    let info = SCROLLINFO {
        cbSize: size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
        nMin: 0,
        nMax: h - 1,
        nPage: actual_height.max(1) as u32,
        nPos: offset,
        nTrackPos: 0,
    };
    SetScrollInfo(dlg.hwnd, SB_VERT, &info, 1);
    // The themed scroll bar has no dark form, and the compacted pages fit, so
    // the bar only shows when there is really something to scroll.
    let overflows = h > actual_height;
    // The themed scroll bar has no dark form on this Windows build (measured
    // white), so dark mode hides it and draws its own thumb instead; light mode
    // keeps the system bar.
    dlg.scroll_range.set((h - actual_height).max(0));
    ShowScrollBar(
        dlg.hwnd,
        SB_VERT,
        if overflows && !dialog_is_dark(dlg) {
            1
        } else {
            0
        },
    );
    if dialog_is_dark(dlg) {
        let mut client: RECT = zeroed();
        if GetClientRect(dlg.hwnd, &mut client) != 0 {
            let strip = RECT {
                left: (client.right - scale(10, dlg.dpi)).max(0),
                top: 0,
                right: client.right,
                bottom: client.bottom,
            };
            InvalidateRect(dlg.hwnd, &strip, 0);
        }
    }
    if offset > 0 {
        let mut child = GetWindow(dlg.hwnd, GW_CHILD);
        while !child.is_null() {
            let mut rect: RECT = zeroed();
            GetWindowRect(child, &mut rect);
            MapWindowPoints(null_mut(), dlg.hwnd, (&mut rect as *mut RECT).cast(), 2);
            SetWindowPos(
                child,
                null_mut(),
                rect.left,
                rect.top - offset,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            child = GetWindow(child, GW_HWNDNEXT);
        }
    }
}

/// Whether Tab walks a `kind` dialog in its reading order: the wizard in both
/// themes, and in Modern the windows its own layout moves into new rows. Every
/// other window keeps its creation order; Settings and the queue manager set
/// their own.
fn follows_reading_order(kind: &DialogKind, modern: bool) -> bool {
    match kind {
        DialogKind::Wizard(_) => true,
        DialogKind::Add(_)
        | DialogKind::Media(_)
        | DialogKind::Progress(_)
        | DialogKind::Rules(_)
        | DialogKind::Crawler(_)
        | DialogKind::Synchronization(_)
        | DialogKind::Logins(_)
        | DialogKind::Events(_)
        | DialogKind::JobLog(_)
        | DialogKind::Speed(_)
        | DialogKind::Rename(_) => modern,
        DialogKind::Settings(_) | DialogKind::Queues(_) | DialogKind::Tools(_) => false,
    }
}

/// The reading order of `rects`, as indexes: row by row from the top, and from
/// the start side within a row. A rectangle joins the row of the topmost one
/// it overlaps vertically.
fn reading_order(rects: &[RECT]) -> Vec<usize> {
    let mut by_top: Vec<usize> = (0..rects.len()).collect();
    by_top.sort_by_key(|&i| (rects[i].top, rects[i].left));
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for i in by_top {
        match rows.last_mut() {
            Some(row) if rects[i].top < rects[row[0]].bottom => row.push(i),
            _ => rows.push(vec![i]),
        }
    }
    rows.into_iter()
        .flat_map(|mut row| {
            row.sort_by_key(|&i| (rects[i].left, rects[i].top));
            row
        })
        .collect()
}

/// Puts a window's tab stops in z-order, which Tab walks, in their reading
/// order. In a right-to-left window the client coordinates are mirrored, so
/// the start side is still the left.
pub(super) unsafe fn tab_in_reading_order(window: HWND) {
    let mut stops = Vec::new();
    let mut rects = Vec::new();
    let mut child = GetWindow(window, GW_CHILD);
    while !child.is_null() {
        if GetWindowLongW(child, GWL_STYLE) as u32 & WS_TABSTOP != 0 {
            let mut rect: RECT = zeroed();
            GetWindowRect(child, &mut rect);
            MapWindowPoints(null_mut(), window, (&mut rect as *mut RECT).cast(), 2);
            stops.push(child);
            rects.push(rect);
        }
        child = GetWindow(child, GW_HWNDNEXT);
    }
    let order = reading_order(&rects);
    if order.iter().enumerate().all(|(place, &i)| place == i) {
        return;
    }
    let ordered: Vec<HWND> = order.iter().map(|&i| stops[i]).collect();
    // Null, when the first tab stop is the first child, is HWND_TOP.
    SetWindowPos(
        ordered[0],
        GetWindow(stops[0], GW_HWNDPREV),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    tab_order(ordered[0], &ordered[1..]);
}

/// Modern's one-field dialogs: the label above the field (`label_lines` lines
/// tall), the field `field_w` epx wide or the full width, the error line
/// under it, and OK and Cancel on the command bar.
unsafe fn layout_one_field(
    dlg: &DialogUi,
    h: i32,
    edit: HWND,
    ok: i32,
    label_lines: i32,
    field_w: Option<i32>,
) {
    let mut client: RECT = zeroed();
    GetClientRect(dlg.hwnd, &mut client);
    let [label, field, error] = one_field_rects(client.right, label_lines, field_w, dlg.dpi);
    let move_to = |control: HWND, rect: RECT| {
        MoveWindow(
            control,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            1,
        );
    };
    if let Some(&control) = child_statics(dlg.hwnd, dlg.error).first() {
        move_to(control, label);
    }
    place_in_frame(edit, field, dlg.dpi);
    move_to(GetDlgItem(dlg.hwnd, D_ERROR), error);
    command_bar(dlg, h, &[(ok, 120), (D_CANCEL, 96)]);
}

/// Where a Modern one-field dialog `width` pixels wide puts its label
/// (`label_lines` lines tall), its field (`field_w` epx wide or the full
/// width) and the error line under it.
fn one_field_rects(width: i32, label_lines: i32, field_w: Option<i32>, dpi: u32) -> [RECT; 3] {
    let s = |value: i32| scale(value, dpi);
    let x = s(ADD_PAD);
    let right = width - x;
    let label = RECT {
        left: x,
        top: s(ADD_TOP),
        right,
        bottom: s(ADD_TOP) + s(ADD_LABEL * label_lines),
    };
    let top = label.bottom + s(ADD_LABEL_GAP);
    let field = RECT {
        left: x,
        top,
        right: field_w.map_or(right, |value| x + s(value)),
        bottom: top + s(ADD_FIELD),
    };
    let error_top = field.bottom + s(ADD_GROUP_GAP);
    let error = RECT {
        left: x,
        top: error_top,
        right,
        bottom: error_top + s(ADD_LABEL),
    };
    [label, field, error]
}

/// Sizes the speed limit dialog and the setup wizard to the lines their first
/// label wraps to, once their controls exist: one in most languages, more
/// where the text is longer.
pub(super) unsafe fn fit_first_label(dlg: &mut DialogUi) {
    let Some(&label) = child_statics(dlg.hwnd, dlg.error).first() else {
        return;
    };
    let (hwnd, font, dpi) = (dlg.hwnd, dlg.font, dlg.dpi);
    // Classic fits only the wizard, whose sides keep the dialog margin.
    let pad = if modern() { ADD_PAD } else { DIALOG_MARGIN };
    let lines = match &mut dlg.kind {
        DialogKind::Speed(f) => &mut f.label_lines,
        DialogKind::Wizard(f) => &mut f.intro_lines,
        _ => return,
    };
    let wrapped = fit_label_lines(hwnd, label, font, dpi, *lines, pad);
    dlg.design.1 += (wrapped - *lines) * ADD_LABEL;
    *lines = wrapped;
    let DialogKind::Wizard(f) = &dlg.kind else {
        return;
    };
    let needed: i32 = wizard_rows(f)
        .iter()
        .map(|cards| wizard_row(dlg, cards).1)
        .sum();
    let extra = needed - f.caption_lines;
    grow_window(hwnd, scale(extra * CARD_LINE, dpi));
    dlg.design.1 += extra * CARD_LINE;
    if let DialogKind::Wizard(f) = &mut dlg.kind {
        f.caption_lines = needed;
    }
}

/// The wizard's rows of cards, top to bottom.
fn wizard_rows(f: &WizardFields) -> [Vec<HWND>; 3] {
    [
        vec![f.simple_card, f.advanced_card],
        vec![f.classic_card, f.modern_card],
        vec![f.video, f.file, f.audio],
    ]
}

/// The face width of each card in a wizard row of `cards`, and the
/// explanation lines beyond one that the row's longest explanation needs.
unsafe fn wizard_row(dlg: &DialogUi, cards: &[HWND]) -> (i32, i32) {
    let s = |value: i32| scale(value, dlg.dpi);
    let modern = modern();
    let mut client: RECT = zeroed();
    GetClientRect(dlg.hwnd, &mut client);
    let x = s(if modern { ADD_PAD } else { DIALOG_MARGIN });
    let ring = if modern { s(FOCUS_MARGIN) } else { 0 };
    let count = cards.len() as i32;
    let width = (client.right - 2 * x - (count - 1) * s(DIALOG_GAP)) / count;
    let lines = cards
        .iter()
        .map(|&card| card_text_lines(card, width + 2 * ring))
        .max()
        .unwrap_or(1);
    (width, lines - 1)
}

/// The wizard's theme heading at (`x`, `y`) and, beside it, its restart note
/// when it has one.
unsafe fn wizard_theme_heading(
    dlg: &DialogUi,
    f: &WizardFields,
    labels: &[HWND],
    x: i32,
    y: i32,
    height: i32,
) {
    let Some(&heading) = labels.get(1) else {
        return;
    };
    let mut client: RECT = zeroed();
    GetClientRect(dlg.hwnd, &mut client);
    let width = text_width(heading) + scale(8, dlg.dpi);
    MoveWindow(heading, x, y, width, height, 1);
    if !f.note.is_null() {
        let left = x + width + scale(DIALOG_GAP, dlg.dpi);
        MoveWindow(f.note, left, y, client.right - x - left, height, 1);
    }
}

/// The setup wizard: the introduction, the depth cards, the theme cards and
/// the purpose cards (the advanced depth's detail) under their headings, and
/// the error line. Modern puts its buttons on the command bar; Classic's go on
/// its button row.
unsafe fn layout_wizard(dlg: &DialogUi, h: i32, f: &WizardFields) {
    let s = |value: i32| scale(value, dlg.dpi);
    let modern = modern();
    let mut client: RECT = zeroed();
    GetClientRect(dlg.hwnd, &mut client);
    let labels = child_statics(dlg.hwnd, dlg.error);
    let (x, top) = if modern {
        (s(ADD_PAD), s(ADD_TOP))
    } else {
        (s(DIALOG_MARGIN), s(DIALOG_MARGIN))
    };
    let cw = client.right - 2 * x;
    let label_gap = s(wizard_label_gap(modern));
    let ring = if modern { s(FOCUS_MARGIN) } else { 0 };
    let gap = s(DIALOG_GAP);
    // A Modern card's window reaches FOCUS_MARGIN past its face for the focus
    // ring. A row is as tall as its longest explanation.
    let row = |cards: &[HWND], show: bool, top: i32| {
        let (width, extra) = wizard_row(dlg, cards);
        let card = s(WIZARD_CARD) + extra * s(CARD_LINE);
        for (index, &card_window) in cards.iter().enumerate() {
            let left = x + index as i32 * (width + gap);
            place(
                card_window,
                show,
                left - ring,
                top - ring,
                width + 2 * ring,
                card + 2 * ring,
            );
        }
        top + card
    };
    let intro = s(ADD_LABEL * f.intro_lines);
    let mut y = top;
    place_static(&labels, 0, x, y, cw, intro, true);
    y += intro + label_gap;
    let [depth, theme, purpose] = wizard_rows(f);
    y = row(&depth, true, y) + gap;
    wizard_theme_heading(dlg, f, &labels, x, y, s(ADD_LABEL));
    y += s(ADD_LABEL) + label_gap;
    y = row(&theme, true, y) + gap;
    // The purpose cards are the advanced answer's detail; a simple choice
    // stays one decision, so they leave the dialog.
    let advanced = check(f.advanced_card);
    place_static(&labels, 2, x, y, cw, s(ADD_LABEL), advanced);
    y += s(ADD_LABEL) + label_gap;
    y = row(&purpose, advanced, y);
    if modern {
        // The error line sits in the content's bottom padding, under the
        // cards' focus margin and above the command bar.
        move_id(dlg.hwnd, D_ERROR, x, y + ring, cw, s(ADD_LABEL));
        command_bar(dlg, h, &[(ID_WIZARD_START, 120), (D_CANCEL, 96)]);
    }
}

/// Modern's log windows: the list under its label, the `actions` on the card
/// below it and `close` on the command bar.
unsafe fn layout_list_dialog(dlg: &DialogUi, h: i32, list: HWND, actions: &[i32], close: i32) {
    let label = child_statics(dlg.hwnd, dlg.error)
        .first()
        .copied()
        .unwrap_or(null_mut());
    let buttons: Vec<Cell> = actions.iter().map(|&id| button_cell(dlg, id)).collect();
    form_rows(
        dlg,
        h,
        &[&[field(label, list, Width::Flex, dlg.dpi).fill()], &buttons],
        0,
    );
    // A log shows more of its lines in compact rows.
    SendMessageW(
        list,
        LB_SETITEMHEIGHT,
        0,
        scale(COMPACT_ROW, dlg.dpi) as isize,
    );
    command_bar(dlg, h, &[(close, 96)]);
}

/// Advanced options' face width: its label and the chevron before it.
unsafe fn advanced_width(dlg: &DialogUi) -> i32 {
    add_button_width(dlg, D_ADVANCED, 120) + scale(16 + 8, dlg.dpi)
}

/// Puts buttons at the left end of a Modern dialog's command bar, left to
/// right as listed, each `width` pixels wide; a hidden button takes no place.
unsafe fn bar_start(dlg: &DialogUi, h: i32, buttons: &[(i32, i32)]) {
    let s = |value: i32| scale(value, dlg.dpi);
    let ring = s(FOCUS_MARGIN);
    let by = h - s(ADD_FOOTER) + s(ADD_PAD);
    let mut left = s(ADD_PAD);
    for &(id, width) in buttons {
        let button = GetDlgItem(dlg.hwnd, id);
        // The style, not IsWindowVisible: the window may not be shown yet.
        if GetWindowLongW(button, GWL_STYLE) as u32 & WS_VISIBLE == 0 {
            continue;
        }
        MoveWindow(
            button,
            left - ring,
            by - ring,
            width + 2 * ring,
            s(ADD_FIELD) + 2 * ring,
            1,
        );
        left += width + s(DIALOG_GAP);
    }
}

/// The media picker's field labels, in the order they are created.
unsafe fn media_labels(dlg: &DialogUi, f: &MediaFields) -> Vec<HWND> {
    child_statics(dlg.hwnd, dlg.error)
        .into_iter()
        .filter(|value| *value != f.thumbnail && *value != f.size)
        .collect()
}

/// The Modern media picker's rows under its summary: the choices it always
/// shows, then its advanced rows while they are open.
unsafe fn media_rows(dlg: &DialogUi, f: &MediaFields) -> Vec<Vec<Cell>> {
    let s = |value: i32| scale(value, dlg.dpi);
    let labels = media_labels(dlg, f);
    let label = |index: usize| labels.get(index).copied().unwrap_or(null_mut());
    let bare = |control: HWND, width: Width| field(null_mut(), control, width, dlg.dpi);
    let combo = |control: HWND| Width::Label(combo_face_width(control, dlg.dpi));
    let lists = list_height(2, dlg.dpi);
    let mut rows = vec![
        vec![field(label(0), f.name, Width::Flex, dlg.dpi)],
        vec![
            field(label(1), f.kind, combo(f.kind), dlg.dpi),
            field(label(2), f.format, Width::Flex, dlg.dpi),
        ],
        vec![Cell {
            height: s(ADD_LABEL),
            ..bare(f.size, Width::Flex)
        }],
        vec![check_cell(dlg, f.remember)],
    ];
    if f.from_start_offered {
        rows.push(vec![check_cell(dlg, f.from_start)]);
    }
    if f.advanced {
        rows.extend([
            vec![
                field(label(3), f.audio, combo(f.audio), dlg.dpi),
                check_cell(dlg, f.playlist),
                bare(
                    f.playlist_items,
                    Width::Fixed(cue_face_width(f.playlist_items, dlg.dpi).max(s(112))),
                ),
            ],
            vec![check_cell(dlg, f.full_verification)],
            vec![
                field(label(4), f.audio_tracks, Width::Flex, dlg.dpi).tall(lists),
                field(label(5), f.subtitles, Width::Flex, dlg.dpi).tall(lists),
            ],
            vec![
                field(label(6), f.external_url, Width::Flex, dlg.dpi),
                bare(
                    f.external_kind,
                    Width::Fixed(combo_face_width(f.external_kind, dlg.dpi)),
                ),
            ],
            vec![
                field(label(7), f.external_language, Width::Label(s(96)), dlg.dpi),
                field(label(8), f.external_label, Width::Flex, dlg.dpi),
                check_cell(dlg, f.external_default),
                button_cell(dlg, ID_MEDIA_EXTERNAL_ADD),
            ],
            vec![
                bare(f.external_list, Width::Flex).tall(lists),
                button_cell(dlg, ID_MEDIA_EXTERNAL_REMOVE),
            ],
            vec![
                bare(f.renew_job, Width::Flex),
                check_cell(dlg, f.renew_restart),
                button_cell(dlg, M_RENEW),
            ],
        ]);
    }
    rows
}

/// Where the Modern media picker's rows start: below its summary.
fn media_rows_top(dpi: u32) -> i32 {
    scale(ADD_TOP + MEDIA_SUMMARY + ADD_GROUP_GAP, dpi)
}

/// The Modern media picker's content height in epx at its window's width:
/// the summary, the rows it shows now, the error line and the command bar.
pub(super) unsafe fn media_content_height(dlg: &DialogUi, f: &MediaFields) -> i32 {
    let s = |value: i32| scale(value, dlg.dpi);
    let mut client: RECT = zeroed();
    GetClientRect(dlg.hwnd, &mut client);
    let x = s(ADD_PAD);
    let rows: i32 = media_rows(dlg, f)
        .iter()
        .map(|row| form_row(dlg, row, x, 0, client.right - 2 * x, 0, false))
        .sum();
    let pixels = media_rows_top(dlg.dpi) + rows + s(ADD_LABEL) + s(ADD_FOOTER);
    (pixels * 96 + dlg.dpi as i32 - 1) / dlg.dpi as i32
}

/// Sizes the Modern media picker's window to the rows it shows now: once its
/// controls exist, when its advanced rows open or close and when its choices
/// arrive.
pub(super) unsafe fn fit_media_picker(dlg: &DialogUi) {
    if let DialogKind::Media(f) = &dlg.kind {
        if modern() {
            resize_dialog(
                dlg.hwnd,
                dlg.dpi,
                media_window_height(media_content_height(dlg, f), 0),
            );
        }
    }
}

/// Puts a Modern dialog's command bar buttons at its right end, left to
/// right as listed, each as wide as its label needs and at least `least` epx.
unsafe fn command_bar(dlg: &DialogUi, h: i32, buttons: &[(i32, i32)]) {
    let s = |value: i32| scale(value, dlg.dpi);
    let mut client: RECT = zeroed();
    GetClientRect(dlg.hwnd, &mut client);
    let ring = s(FOCUS_MARGIN);
    let by = h - s(ADD_FOOTER) + s(ADD_PAD);
    let mut right = client.right - s(ADD_PAD);
    for &(id, least) in buttons.iter().rev() {
        let bw = add_button_width(dlg, id, least);
        move_id(
            dlg.hwnd,
            id,
            right - bw - ring,
            by - ring,
            bw + 2 * ring,
            s(ADD_FIELD) + 2 * ring,
        );
        right -= bw + s(DIALOG_GAP);
    }
}

/// How wide a control in a Modern form row is.
#[derive(Clone, Copy)]
enum Width {
    /// An equal share of what the row's other cells leave.
    Flex,
    /// Exactly this many pixels.
    Fixed(i32),
    /// As wide as its label and at least this many pixels. A row short of
    /// room narrows the widest of these first, down to that least, and their
    /// labels wrap.
    Label(i32),
}

/// A control in a Modern form row and the label above it, if any. A height of
/// 0 takes what the dialog has left.
struct Cell {
    label: HWND,
    control: HWND,
    width: Width,
    height: i32,
}

impl Cell {
    fn fill(self) -> Cell {
        Cell { height: 0, ..self }
    }

    fn tall(self, height: i32) -> Cell {
        Cell { height, ..self }
    }
}

fn field(label: HWND, control: HWND, width: Width, dpi: u32) -> Cell {
    Cell {
        label,
        control,
        width,
        height: scale(ADD_FIELD, dpi),
    }
}

/// A push button on the card, as wide as its label needs and at least 96 epx.
unsafe fn button_cell(dlg: &DialogUi, id: i32) -> Cell {
    field(
        null_mut(),
        GetDlgItem(dlg.hwnd, id),
        Width::Fixed(add_button_width(dlg, id, 96)),
        dlg.dpi,
    )
}

/// A check box as wide as its box and label.
unsafe fn check_cell(dlg: &DialogUi, control: HWND) -> Cell {
    field(
        null_mut(),
        control,
        Width::Fixed(text_width(control) + check_text_inset(dlg.dpi)),
        dlg.dpi,
    )
}

/// The height a label needs at `width`: its wrapped lines, ADD_LABEL each.
unsafe fn label_height(dlg: &DialogUi, label: HWND, width: i32) -> i32 {
    wrapped_height(label, dlg.font, dlg.dpi, width)
}

/// `label_height` in `font` at `dpi`.
unsafe fn wrapped_height(label: HWND, font: HFONT, dpi: u32, width: i32) -> i32 {
    let mut value: Vec<u16> = text(label).encode_utf16().collect();
    let dc = GetDC(label);
    let previous = SelectObject(dc, font as HGDIOBJ);
    let mut line = RECT {
        right: width,
        ..zeroed()
    };
    let mut probe: Vec<u16> = "Ag".encode_utf16().collect();
    DrawTextW(
        dc,
        probe.as_mut_ptr(),
        probe.len() as i32,
        &mut line,
        DT_CALCRECT | DT_SINGLELINE,
    );
    let mut all = RECT {
        right: width,
        ..zeroed()
    };
    draw_text(
        dc,
        &mut value,
        &mut all,
        DT_CALCRECT | DT_WORDBREAK | DT_EXPANDTABS | DT_NOPREFIX,
    );
    SelectObject(dc, previous);
    ReleaseDC(label, dc);
    let line = line.bottom.max(1);
    ((all.bottom + line - 1) / line).max(1) * scale(ADD_LABEL, dpi)
}

/// The space before a cell: 8 epx before a push button, 12 before the rest.
unsafe fn cell_gap(dlg: &DialogUi, cell: &Cell) -> i32 {
    let button =
        window_class(cell.control).eq_ignore_ascii_case("Button") && !is_check_box(cell.control);
    scale(if button { DIALOG_GAP } else { ADD_GROUP_GAP }, dlg.dpi)
}

/// The cells' widths in a row `room` pixels wide.
unsafe fn cell_widths(dlg: &DialogUi, cells: &[Cell], room: i32) -> Vec<i32> {
    let gaps: i32 = cells.iter().skip(1).map(|cell| cell_gap(dlg, cell)).sum();
    let sized: Vec<(Width, i32)> = cells
        .iter()
        .map(|cell| {
            let label = match cell.width {
                Width::Label(_) => text_width(cell.label),
                _ => 0,
            };
            (cell.width, label)
        })
        .collect();
    share_row(&sized, room - gaps)
}

/// Splits `room` between a row's cells, each given with its label's width:
/// fixed cells keep their width, label cells start as wide as their label
/// and, while the row is too wide, the widest of them narrow together down to
/// their least, and flexible cells share what is left.
fn share_row(cells: &[(Width, i32)], room: i32) -> Vec<i32> {
    let capped = |cap: i32| -> Vec<i32> {
        cells
            .iter()
            .map(|&(width, label)| match width {
                Width::Label(least) => label.min(cap).max(least),
                Width::Fixed(width) => width,
                Width::Flex => 0,
            })
            .collect()
    };
    let mut cap = cells.iter().map(|&(_, label)| label).max().unwrap_or(0);
    let mut widths = capped(cap);
    while cap > 0 && widths.iter().sum::<i32>() > room {
        cap -= 1;
        widths = capped(cap);
    }
    let flex = cells
        .iter()
        .filter(|(width, _)| matches!(width, Width::Flex))
        .count() as i32;
    if flex > 0 {
        let share = (room - widths.iter().sum::<i32>()).max(0) / flex;
        for (width, (kind, _)) in widths.iter_mut().zip(cells) {
            if matches!(kind, Width::Flex) {
                *width = share;
            }
        }
    }
    widths
}

/// Lays a Modern form row out at `y`, from `x` across `room` pixels: the
/// labels above their controls, as tall as the row's longest wrapped label,
/// and each control on its face. A cell of height 0 is `fill` tall. Returns
/// the row's height with the space below it; with `place` false it only
/// measures.
unsafe fn form_row(
    dlg: &DialogUi,
    cells: &[Cell],
    x: i32,
    y: i32,
    room: i32,
    fill: i32,
    place: bool,
) -> i32 {
    let s = |value: i32| scale(value, dlg.dpi);
    let widths = cell_widths(dlg, cells, room);
    let label_h = cells
        .iter()
        .zip(&widths)
        .filter(|(cell, _)| !cell.label.is_null())
        .map(|(cell, &width)| label_height(dlg, cell.label, width))
        .max()
        .unwrap_or(0);
    let top = if label_h > 0 {
        y + label_h + s(ADD_LABEL_GAP)
    } else {
        y
    };
    let height_of = |cell: &Cell| if cell.height == 0 { fill } else { cell.height };
    if place {
        let mut left = x;
        for (index, (cell, &width)) in cells.iter().zip(&widths).enumerate() {
            if index > 0 {
                left += cell_gap(dlg, cell);
            }
            if !cell.label.is_null() {
                MoveWindow(cell.label, left, y, width, label_h, 1);
            }
            let face = RECT {
                left,
                top,
                right: left + width,
                bottom: top + height_of(cell),
            };
            place_modern(cell.control, true, face, dlg.dpi);
            left += width;
        }
    }
    top - y + cells.iter().map(height_of).max().unwrap_or(0) + s(ADD_GROUP_GAP)
}

/// Lays a Modern list and form dialog's rows out down its card, with the
/// error line `reserve` pixels below them. The cell of height 0 takes the
/// height the card has left, at least two list entries. Returns where the
/// rows end.
unsafe fn form_rows(dlg: &DialogUi, h: i32, rows: &[&[Cell]], reserve: i32) -> i32 {
    form_rows_from(dlg, h, scale(ADD_TOP, dlg.dpi), rows, reserve)
}

/// `form_rows` from `top` pixels down instead of the card's first row.
unsafe fn form_rows_from(dlg: &DialogUi, h: i32, top: i32, rows: &[&[Cell]], reserve: i32) -> i32 {
    let s = |value: i32| scale(value, dlg.dpi);
    let mut client: RECT = zeroed();
    GetClientRect(dlg.hwnd, &mut client);
    let x = s(ADD_PAD);
    let room = client.right - 2 * x;
    let natural: i32 = rows
        .iter()
        .map(|row| form_row(dlg, row, x, 0, room, least_fill(dlg.dpi), false))
        .sum();
    let fill = fill_height(h, top, natural, reserve, dlg.dpi);
    let mut y = top;
    for row in rows {
        y += form_row(dlg, row, x, y, room, fill, true);
    }
    move_id(dlg.hwnd, D_ERROR, x, y + reserve, room, s(ADD_LABEL));
    y
}

/// The height of a Modern list's face that shows `rows` whole entries inside
/// its frame.
fn list_height(rows: i32, dpi: u32) -> i32 {
    rows * scale(ROW_HEIGHT, dpi) + 2 * scale(LIST_INSET, dpi)
}

/// The least height of a form's fill cell: two list entries in their frame.
fn least_fill(dpi: u32) -> i32 {
    list_height(2, dpi)
}

/// The height of a form's fill cell: its least, and what a card `h` pixels
/// tall leaves below the rows' `natural` height (measured with the least),
/// starting at `top`, with the error line `reserve` pixels below them.
fn fill_height(h: i32, top: i32, natural: i32, reserve: i32, dpi: u32) -> i32 {
    let spare = h - scale(ADD_FOOTER, dpi) - scale(ADD_LABEL, dpi) - reserve - top - natural;
    least_fill(dpi) + spare.max(0)
}

/// Grows `window` by the lines `label` wraps to beyond `lines` at the card's
/// width, `pad` epx inside each side, ADD_LABEL each, and returns the lines it
/// wraps to.
unsafe fn fit_label_lines(
    window: HWND,
    label: HWND,
    font: HFONT,
    dpi: u32,
    lines: i32,
    pad: i32,
) -> i32 {
    let mut client: RECT = zeroed();
    GetClientRect(window, &mut client);
    let room = client.right - 2 * scale(pad, dpi);
    let wrapped = wrapped_height(label, font, dpi, room) / scale(ADD_LABEL, dpi);
    grow_window(window, scale((wrapped - lines) * ADD_LABEL, dpi));
    wrapped
}

/// Makes `window` `pixels` taller, or shorter for a negative count.
unsafe fn grow_window(window: HWND, pixels: i32) {
    if pixels == 0 {
        return;
    }
    let mut frame: RECT = zeroed();
    GetWindowRect(window, &mut frame);
    SetWindowPos(
        window,
        null_mut(),
        0,
        0,
        frame.right - frame.left,
        frame.bottom - frame.top + pixels,
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

#[cfg(test)]
mod tests {
    use super::{share_row, Width};

    #[test]
    fn a_form_s_fill_cell_keeps_two_list_entries_and_takes_what_is_left() {
        use super::{fill_height, scale, ADD_FOOTER, ADD_LABEL, LIST_INSET, ROW_HEIGHT};
        for dpi in [96, 120, 144, 168] {
            let s = |value: i32| scale(value, dpi);
            // Rows taller than the card: the cell keeps its least, and the
            // list placed inside its frame still shows two whole entries.
            let least = fill_height(s(300), s(20), s(600), 0, dpi);
            assert!(least - 2 * s(LIST_INSET) >= 2 * s(ROW_HEIGHT), "{dpi} dpi");
            // Room to spare: the cell takes all of it.
            let h = s(500);
            let (top, natural, reserve) = (s(20), s(200), s(12));
            let spare = h - s(ADD_FOOTER) - s(ADD_LABEL) - reserve - top - natural;
            assert!(spare > 0);
            assert_eq!(fill_height(h, top, natural, reserve, dpi), least + spare);
        }
    }

    #[test]
    fn a_label_that_wraps_grows_its_window_by_its_extra_lines() {
        use super::*;
        unsafe {
            let font = ramp_font(96, BODY_PX, false);
            for (value, wraps) in [
                ("Speed limit", false),
                (
                    "The limit applies to each selected download separately, in kilobytes per second",
                    true,
                ),
            ] {
                let window = control(null_mut(), "STATIC", "", WS_POPUP, 0, 0);
                SetWindowPos(window, null_mut(), 0, 0, 300, 200, SWP_NOZORDER | SWP_NOACTIVATE);
                let label = control(window, "STATIC", value, WS_CHILD, 0, 1);
                apply_font(label, font);
                let lines = fit_label_lines(window, label, font, 96, 1, ADD_PAD);
                assert_eq!(lines > 1, wraps, "{value}: {lines} lines");
                let mut frame: RECT = zeroed();
                GetWindowRect(window, &mut frame);
                assert_eq!(frame.bottom - frame.top, 200 + (lines - 1) * ADD_LABEL);
                // Measured again at its lines, the window stays as it is.
                assert_eq!(fit_label_lines(window, label, font, 96, lines, ADD_PAD), lines);
                GetWindowRect(window, &mut frame);
                assert_eq!(frame.bottom - frame.top, 200 + (lines - 1) * ADD_LABEL);
                DestroyWindow(window);
            }
            DeleteObject(font as HGDIOBJ);
        }
    }

    #[test]
    fn a_one_field_dialog_keeps_the_rhythm_and_ends_its_error_line_at_the_bar() {
        use super::{one_field_height, one_field_rects, scale, ADD_FOOTER, RECT};
        for dpi in [96, 120, 144] {
            let s = |value: i32| scale(value, dpi);
            for (lines, field_w) in [(1, None), (2, Some(140))] {
                let width = s(480);
                let [label, field, error] = one_field_rects(width, lines, field_w, dpi);
                let height = |rect: RECT| rect.bottom - rect.top;
                assert_eq!((label.left, label.top), (s(24), s(20)));
                assert_eq!(height(label), s(20) * lines);
                assert_eq!(field.top - label.bottom, s(8));
                assert_eq!(height(field), s(32));
                assert_eq!(
                    field.right - field.left,
                    field_w.map_or(width - 2 * s(24), s)
                );
                assert_eq!(error.top - field.bottom, s(12));
                assert_eq!((label.right, error.right), (width - s(24), width - s(24)));
                // The content height leaves the error line ending where the
                // command bar starts.
                assert_eq!(
                    error.bottom,
                    s(one_field_height(lines)) - s(ADD_FOOTER),
                    "{dpi} dpi, {lines} lines"
                );
            }
        }
    }

    #[test]
    fn reading_order_goes_row_by_row_from_the_start_side() {
        use super::{reading_order, RECT};
        let rect = |left, top, width, height| RECT {
            left,
            top,
            right: left + width,
            bottom: top + height,
        };
        let rects = [
            // An edit's text area and the taller button beside it.
            rect(35, 158, 433, 19),
            rect(481, 149, 102, 38),
            // A check box under them.
            rect(21, 272, 368, 32),
            // The command bar, its start-side button last.
            rect(195, 360, 150, 38),
            rect(453, 360, 126, 38),
            rect(21, 360, 172, 38),
            rect(581, 360, 102, 38),
        ];
        assert_eq!(reading_order(&rects), vec![0, 1, 2, 5, 3, 4, 6]);
    }

    #[test]
    fn tab_follows_the_reading_order_and_leaves_the_labels() {
        use super::*;
        unsafe {
            for ex in [0, WS_EX_LAYOUTRTL] {
                let window = control(null_mut(), "STATIC", "", WS_POPUP, ex, 0);
                SetWindowPos(
                    window,
                    null_mut(),
                    0,
                    0,
                    400,
                    200,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                let mut made = Vec::new();
                // Created out of order: (left, top) in client coordinates.
                for (id, left, top) in [(3, 0, 100), (2, 200, 0), (9, 0, 70), (1, 0, 0)] {
                    let style = if id == 9 {
                        WS_CHILD
                    } else {
                        WS_CHILD | WS_TABSTOP
                    };
                    let child = control(window, "BUTTON", "", style, 0, id);
                    SetWindowPos(
                        child,
                        null_mut(),
                        left,
                        top,
                        80,
                        30,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    made.push(child);
                }
                tab_in_reading_order(window);
                let mut walk = Vec::new();
                let mut child = GetWindow(window, GW_CHILD);
                while !child.is_null() {
                    walk.push(GetDlgCtrlID(child));
                    child = GetWindow(child, GW_HWNDNEXT);
                }
                DestroyWindow(window);
                let stops: Vec<i32> = walk.iter().copied().filter(|&id| id != 9).collect();
                assert_eq!(stops, vec![1, 2, 3], "layout {ex:#x}: {walk:?}");
                assert!(walk.contains(&9));
            }
        }
    }

    #[test]
    fn a_short_row_narrows_its_widest_labels_first_and_shares_the_rest() {
        let row = [
            (Width::Label(96), 150),
            (Width::Label(160), 215),
            (Width::Label(96), 230),
            (Width::Fixed(165), 0),
        ];
        // Room for every label on one line.
        assert_eq!(share_row(&row, 800), vec![150, 215, 230, 165]);
        // 44 px short: the two widest narrow to the same width, the
        // narrowest label and the fixed cell keep theirs.
        assert_eq!(share_row(&row, 716), vec![150, 200, 200, 165]);
        // Far too short: each label cell stops at its least.
        assert_eq!(share_row(&row, 100), vec![96, 160, 96, 165]);
        // Flexible cells split what the others leave, never below nothing.
        let shared = [(Width::Flex, 0), (Width::Fixed(100), 0), (Width::Flex, 0)];
        assert_eq!(share_row(&shared, 500), vec![200, 100, 200]);
        assert_eq!(share_row(&shared, 50), vec![0, 100, 0]);
    }
}

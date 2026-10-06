//! Placement of every dialog kind's controls for the current size and DPI.

use super::*;

pub(super) unsafe fn layout_dialog(dlg: &DialogUi) {
    let mut r: RECT = zeroed();
    GetClientRect(dlg.hwnd, &mut r);
    let w = r.right;
    let actual_height = r.bottom;
    let logical_height = match &dlg.kind {
        DialogKind::Wizard(_) => 372,
        DialogKind::Add(f) => {
            if f.advanced {
                372
            } else {
                292
            }
        }
        DialogKind::Settings(f) => {
            settings_logical_height(UiMode::parse(&f.value.ui_mode).is_simple())
        }
        DialogKind::Tools(_) => 425,
        DialogKind::Queues(_) => 425,
        DialogKind::Rules(_) => 425,
        DialogKind::Crawler(_) => 425,
        DialogKind::Synchronization(_) => 425,
        DialogKind::Events(_) => 425,
        DialogKind::JobLog(_) => 425,
        DialogKind::Speed(_) => 110,
        DialogKind::Rename(_) => 110,
        DialogKind::Logins(_) => 425,
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
            let labels = child_statics(dlg.hwnd, dlg.error);
            if let Some(label) = labels.first() {
                MoveWindow(*label, m, m, w - 2 * m, scale(20, dlg.dpi), 1);
            }
            // Cards are sized to their own content (icon, title, explanation)
            // and centred in the body, instead of being stretched to whatever
            // height the body happens to have.
            let advanced = check(f.advanced_card);
            let card_h = scale(112, dlg.dpi);
            let card_gap = gap * 2;
            let mode_w = (w - 2 * m - gap) / 2;
            let mode_top = m + scale(28, dlg.dpi);
            place(f.simple_card, true, m, mode_top, mode_w, card_h);
            place(
                f.advanced_card,
                true,
                m + mode_w + gap,
                mode_top,
                mode_w,
                card_h,
            );
            // The purpose cards are the advanced answer's detail; a simple
            // choice stays one decision, so they leave the dialog entirely.
            let heading_y = mode_top + card_h + gap;
            place_static(
                &labels,
                1,
                m,
                heading_y + scale(3, dlg.dpi),
                w - 2 * m,
                scale(18, dlg.dpi),
                advanced,
            );
            let cards = [f.video, f.file, f.audio];
            let card_w = (w - 2 * m - 2 * card_gap) / 3;
            let top = heading_y + scale(22, dlg.dpi);
            for (index, control) in cards.iter().enumerate() {
                place(
                    *control,
                    advanced,
                    m + index as i32 * (card_w + card_gap),
                    top,
                    card_w,
                    card_h,
                );
            }
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
        DialogKind::Add(f) => {
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
            let label_w = scale(210, dlg.dpi);
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
                w - m - scale(130, dlg.dpi),
                by,
                scale(130, dlg.dpi),
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
            move_id(
                dlg.hwnd,
                S_TABS,
                m,
                m,
                scale(430, dlg.dpi),
                scale(30, dlg.dpi),
            );
            let cx = m + scale(2, dlg.dpi);
            let cw = w - 2 * cx;
            let cy = m + scale(38, dlg.dpi);
            // One rhythm for both tabs: 22 px controls on a 26 px pitch, so no
            // row ever touches the next one, and one label column.
            let line = scale(22, dlg.dpi);
            let step = scale(26, dlg.dpi);
            let label_w = scale(200, dlg.dpi);
            let field_x = cx + label_w;
            let small = scale(72, dlg.dpi);
            let half = (cw - gap) / 2;

            // Genel: every control stays a direct child of the dialog, so
            // GetDlgItem keeps answering while the other tab is selected. A
            // row the mode does not have is skipped whole - the cursor only
            // moves for a row that was laid out - so neither surface is left
            // with the empty place of a hidden control.
            let mut row_y = cy;
            // Arayüz düzeyi: which rows this page has at all. It is the first
            // row of the page it decides, and it stays reachable in both modes.
            place_static(
                &labels,
                11,
                cx,
                row_y + scale(3, dlg.dpi),
                label_w,
                scale(18, dlg.dpi),
                genel,
            );
            place(f.ui_mode, genel, field_x, row_y, scale(140, dlg.dpi), line);
            // Arayüz dili shares the mode row: the label column belongs to it,
            // so this label sits in the field area beside the combo.
            place_static(
                &labels,
                12,
                field_x + scale(148, dlg.dpi),
                row_y + scale(3, dlg.dpi),
                scale(78, dlg.dpi),
                scale(18, dlg.dpi),
                genel,
            );
            place(
                f.ui_language,
                genel,
                field_x + scale(230, dlg.dpi),
                row_y,
                cw - label_w - scale(230, dlg.dpi),
                line,
            );
            row_y += step;

            place_static(
                &labels,
                0,
                cx,
                row_y + scale(3, dlg.dpi),
                label_w,
                scale(18, dlg.dpi),
                genel,
            );
            place(
                f.dir,
                genel,
                field_x,
                row_y,
                cw - label_w - scale(92, dlg.dpi),
                line,
            );
            place(
                GetDlgItem(dlg.hwnd, S_BROWSE),
                genel,
                cx + cw - scale(88, dlg.dpi),
                row_y,
                scale(88, dlg.dpi),
                line,
            );
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
                    y + scale(3, dlg.dpi),
                    label_w,
                    scale(18, dlg.dpi),
                    simple_transfer,
                );
                place(*control, simple_transfer, field_x, y, small, line);
            }
            if simple {
                row_y += step * 2;
            }

            // Kullanım amacı: part of the advanced answer only, so the simple
            // page has neither the row nor its label.
            let purpose = genel && !simple;
            place_static(
                &labels,
                1,
                cx,
                row_y + scale(3, dlg.dpi),
                label_w,
                scale(18, dlg.dpi),
                purpose,
            );
            let mode_w = scale(58, dlg.dpi);
            for (i, control) in [f.mode_video, f.mode_file, f.mode_audio].iter().enumerate() {
                place(
                    *control,
                    purpose,
                    field_x + mode_w * i as i32,
                    row_y,
                    mode_w,
                    line,
                );
            }
            if !simple {
                row_y += step;
            }

            // Pano and başlangıç share a row in advanced mode. In simple mode
            // the clipboard watch is gone, so the two remaining checkboxes take
            // the row - and the site-entry toggle, whose label is the longest,
            // gets the wider half of it.
            let clipboard = genel && !simple;
            place(f.clipboard, clipboard, cx, row_y, half, line);
            if clipboard {
                place(f.startup, genel, cx + half + gap, row_y, half, line);
            } else {
                let site_w = half + scale(20, dlg.dpi);
                place(f.site_entries, genel, cx, row_y, site_w, line);
                place(
                    f.startup,
                    genel,
                    cx + site_w + gap,
                    row_y,
                    cw - site_w - gap,
                    line,
                );
            }
            row_y += step;
            place(f.tray, genel, cx, row_y, cw, line);
            row_y += step;
            place(f.notify, genel, cx, row_y, cw, line);
            row_y += step;
            place(f.dark, genel, cx, row_y, cw, line);
            row_y += step;
            if !simple {
                place(f.site_entries, genel, cx, row_y, cw, line);
                row_y += step;
            }

            // The schedule toggle and its two clock fields share one row, and
            // all three are advanced-only.
            let schedule = genel && !simple;
            let clock_w = scale(64, dlg.dpi);
            place(f.schedule, schedule, cx, row_y, scale(238, dlg.dpi), line);
            place_static(
                &labels,
                2,
                cx + scale(246, dlg.dpi),
                row_y + scale(3, dlg.dpi),
                scale(62, dlg.dpi),
                scale(18, dlg.dpi),
                schedule,
            );
            place(
                f.start,
                schedule,
                cx + scale(310, dlg.dpi),
                row_y,
                clock_w,
                line,
            );
            place_static(
                &labels,
                3,
                cx + scale(382, dlg.dpi),
                row_y + scale(3, dlg.dpi),
                scale(38, dlg.dpi),
                scale(18, dlg.dpi),
                schedule,
            );
            place(
                f.end,
                schedule,
                cx + scale(424, dlg.dpi),
                row_y,
                clock_w,
                line,
            );
            if !simple {
                row_y += step;
            }
            place(f.update_check, genel, cx, row_y, cw, line);
            row_y += step;
            place(
                GetDlgItem(dlg.hwnd, ID_SETTINGS_CHECK_UPDATES),
                genel,
                cx,
                row_y,
                scale(232, dlg.dpi),
                line,
            );
            place(
                GetDlgItem(dlg.hwnd, ID_SETTINGS_INSTALL_EXTENSION),
                genel,
                cx + scale(240, dlg.dpi),
                row_y,
                scale(232, dlg.dpi),
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
                        y + scale(3, dlg.dpi),
                        label_w,
                        scale(18, dlg.dpi),
                        advanced,
                    );
                    // A two or three digit value needs no 120 px field.
                    place(*control, advanced, field_x, y, small, line);
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
                        cy + step * i as i32 + scale(3, dlg.dpi),
                        label_w,
                        scale(18, dlg.dpi),
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
                    y + scale(3, dlg.dpi),
                    label_w,
                    scale(18, dlg.dpi),
                    advanced,
                );
                place(*control, advanced, field_x, y, small, line);
            }
            let browser_y = cy + step * 6;
            place(
                f.browser_transfer,
                advanced,
                cx,
                browser_y,
                cw,
                scale(38, dlg.dpi),
            );
            let log_y = browser_y + scale(46, dlg.dpi);
            place_static(
                &labels,
                10,
                cx,
                log_y + scale(3, dlg.dpi),
                label_w,
                scale(18, dlg.dpi),
                advanced,
            );
            place(
                f.log_level,
                advanced,
                field_x,
                log_y,
                scale(140, dlg.dpi),
                line,
            );
            // The host-log toggle shares the row with the level it belongs to
            // instead of pushing another row into the error line.
            place(
                f.log_hosts,
                advanced,
                field_x + scale(152, dlg.dpi),
                log_y,
                cw - label_w - scale(152, dlg.dpi),
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
                place_static(
                    &labels,
                    *label,
                    cx,
                    y + scale(3, dlg.dpi),
                    label_w,
                    scale(18, dlg.dpi),
                    network,
                );
                let width = if i == 0 {
                    scale(160, dlg.dpi)
                } else {
                    cw - label_w
                };
                place(*control, network, field_x, y, width, line);
            }
            let limit_y = cy + step * 4;
            let clock_w = scale(64, dlg.dpi);
            place(
                f.speed_schedule,
                network,
                cx,
                limit_y,
                scale(238, dlg.dpi),
                line,
            );
            place_static(
                &labels,
                19,
                cx + scale(246, dlg.dpi),
                limit_y + scale(3, dlg.dpi),
                scale(62, dlg.dpi),
                scale(18, dlg.dpi),
                network,
            );
            place(
                f.speed_start,
                network,
                cx + scale(310, dlg.dpi),
                limit_y,
                clock_w,
                line,
            );
            place_static(
                &labels,
                20,
                cx + scale(382, dlg.dpi),
                limit_y + scale(3, dlg.dpi),
                scale(38, dlg.dpi),
                scale(18, dlg.dpi),
                network,
            );
            place(
                f.speed_end,
                network,
                cx + scale(424, dlg.dpi),
                limit_y,
                clock_w,
                line,
            );
            place_static(
                &labels,
                21,
                cx,
                limit_y + step + scale(3, dlg.dpi),
                label_w,
                scale(18, dlg.dpi),
                network,
            );
            place(f.speed_kib, network, field_x, limit_y + step, small, line);
            // Davranış: switches, then the two labelled rows.
            let behaviour = dlg.tab.get() == 3 && !simple;
            for (i, control) in [
                f.keep_awake,
                f.sound,
                f.completion_card,
                f.takeover,
                f.metered,
                f.hotkey,
            ]
            .iter()
            .enumerate()
            {
                place(*control, behaviour, cx, cy + step * i as i32, cw, line);
            }
            let choice_y = cy + step * 6;
            place_static(
                &labels,
                22,
                cx,
                choice_y + scale(3, dlg.dpi),
                scale(300, dlg.dpi),
                scale(18, dlg.dpi),
                behaviour,
            );
            place(
                f.double_click,
                behaviour,
                cx + scale(304, dlg.dpi),
                choice_y,
                scale(170, dlg.dpi),
                line,
            );
            place_static(
                &labels,
                23,
                cx,
                choice_y + step + scale(3, dlg.dpi),
                scale(300, dlg.dpi),
                scale(18, dlg.dpi),
                behaviour,
            );
            place(
                f.history,
                behaviour,
                cx + scale(304, dlg.dpi),
                choice_y + step,
                small,
                line,
            );

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
            field(1, 0, in_row(0), 40, line);
            MoveWindow(f.name, x(44), in_row(0), cw - scale(300, dlg.dpi), line, 1);
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
            field(3, 0, in_row(2), 90, line);
            MoveWindow(f.completion, x(94), in_row(2), scale(200, dlg.dpi), line, 1);
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
                x(420),
                in_row(2),
                scale(110, dlg.dpi),
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
            MoveWindow(f.enabled, x(230), in_row(2), scale(150, dlg.dpi), line, 1);
            MoveWindow(f.overwrite, x(390), in_row(2), scale(270, dlg.dpi), line, 1);
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
        DialogKind::Media(f) => {
            // The thumbnail is a STATIC too, but it is placed on its own below,
            // not as one of the field labels.
            let labels: Vec<_> = child_statics(dlg.hwnd, dlg.error)
                .into_iter()
                .filter(|value| *value != f.thumbnail)
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
            let format_bottom = if f.advanced {
                advanced_top - gap
            } else {
                by - scale(29, dlg.dpi) - gap
            } - line
                - gap;
            // The remembered-choice switch sits right under the format list.
            MoveWindow(f.remember, m, format_bottom + gap, cw, line, 1);
            MoveWindow(
                f.format,
                m,
                format_top,
                cw,
                (format_bottom - format_top).max(scale(120, dlg.dpi)),
                1,
            );
            EnableWindow(f.format, (!audio_only) as BOOL);
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

// AI Usage Bar — GNOME Shell indicator that renders ai-usagebar's
// 5-hour (session), weekly, and (optionally) extra-usage bars in the top
// panel next to the clock/network, with a native, aligned dropdown.
//
// It shells out to the `ai-usagebar` binary (always exits 0, emits Waybar
// JSON `{text, tooltip, class}`) and draws everything with native St
// widgets. Bar colors and thresholds default to the binary's One Dark
// theme but are user-configurable.

import GObject from 'gi://GObject';
import St from 'gi://St';
import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {barMarkup, colorForPct, field, FIELD, FORMAT, hasUsageWindows, integer,
    isGrouped, isStaleFormatOutput, markerElapsed, plainTextFromPango,
    splitFormatOutput} from './marker-logic.js';

const ROLE = 'ai-usagebar';

// Fixed accent colors (tags / dim text). Bar colors are user-configurable.
const DIM = '#5c6370';
const FG = '#abb2bf';
const RED = '#e06c75';
// FORMAT's final ignored literal sentinel receives a stale suffix, keeping the
// preceding elapsed fields numeric. It and its field indexes live in marker-logic.
const REFRESH_TIMEOUT_SECS = 60;

function esc(s) {
    return String(s)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;');
}

function resolveBinary(settings) {
    const configured = settings.get_string('binary-path');
    if (configured && GLib.file_test(configured, GLib.FileTest.IS_EXECUTABLE))
        return configured;
    const onPath = GLib.find_program_in_path('ai-usagebar');
    if (onPath)
        return onPath;
    const cargo = `${GLib.get_home_dir()}/.cargo/bin/ai-usagebar`;
    if (GLib.file_test(cargo, GLib.FileTest.IS_EXECUTABLE))
        return cargo;
    return 'ai-usagebar';
}

const Indicator = GObject.registerClass(
class AiUsageBarIndicator extends PanelMenu.Button {
    _init(settings, openPrefs, fixedVendor) {
        super._init(0.0, 'AI Usage Bar', false);

        this._settings = settings;
        this._openPrefs = openPrefs;
        this._fixedVendor = fixedVendor;
        this._data = null;          // parsed snapshot for redraws
        this._busy = false;
        // A refresh asked for while one was in flight, to run once it settles.
        this._refreshPending = false;
        this._timer = 0;
        this._refreshTimeoutId = 0;
        this._refreshCancellable = null;
        this._refreshProc = null;
        this._refreshToken = 0;
        this._rows = {};

        // Panel: provider image icon + existing markup label.
        this._panelBox = new St.BoxLayout({
            y_align: Clutter.ActorAlign.CENTER,
        });

        const iconNames = {
            openai: 'openai.svg',
            openrouter: 'openrouter.png',
            antigravity: 'gemini.png',
        };

        const iconName = iconNames[this._fixedVendor];
        if (iconName) {
            const iconPath = GLib.build_filenamev([
                GLib.path_get_dirname(GLib.filename_from_uri(import.meta.url)[0]),
                'icons',
                iconName,
            ]);

            this._providerIcon = new St.Icon({
                gicon: Gio.icon_new_for_string(iconPath),
                icon_size: 16,
                y_align: Clutter.ActorAlign.CENTER,
                style_class: 'aiub-provider-icon',
            });

            this._panelBox.add_child(this._providerIcon);
        }

        this._label = new St.Label({
            text: '…',
            y_align: Clutter.ActorAlign.CENTER,
            style_class: 'aiub-label',
        });

        this._panelBox.add_child(this._label);
        this.add_child(this._panelBox);

        this._buildMenu();

        // Re-render cached data when any display setting changes (no refetch).
        const viewKeys = [
            'bar-width', 'show-percent', 'show-bars', 'show-session',
            'show-weekly', 'show-extra', 'color-low', 'color-mid',
            'color-high', 'color-critical', 'color-empty',
        ];
        this._viewIds = viewKeys.map(k =>
            this._settings.connect(`changed::${k}`, () => this._render()));

        this._intervalId = this._settings.connect('changed::refresh-interval',
            () => this._restartTimer());
        this._sourceIds = [
            this._settings.connect('changed::binary-path', () => this._refresh()),
        ];

        this.menu.connect('open-state-changed', (_m, open) => {
            if (open)
                this._refresh();
        });

        this._refresh();
        this._restartTimer();
    }

    _buildMenu(grouped = false) {
        this.menu.removeAll();
        this._rows = {};
        this._grouped = grouped;

        // Header (plan name).
        const header = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        this._planLabel = new St.Label({text: 'AI Usage', x_expand: true, style_class: 'aiub-header'});
        header.add_child(this._planLabel);
        this.menu.addMenuItem(header);

        if (grouped) {
            // Two independent quota pools per window type. Row order changes,
            // but the data mapping does not: session/weekly still hold the
            // primary pool, so the panel bar and the show-session/show-weekly
            // toggles keep working exactly as they do for every other vendor.
            this._addHeading('5 小時額度');
            this._addRow('session', '5 小時額度');
            this._addRow('sonnet', 'Sonnet 專用額度');
            this._addHeading('每週額度');
            this._addRow('weekly', '每週額度');
            this._addRow('extra', '額外使用量');
        } else {
            this._addRow('session', '5 小時額度');
            this._addRow('weekly', '每週額度');
            this._addRow('sonnet', 'Sonnet 專用額度');
            this._addRow('extra', '額外使用量');
        }

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        const refreshItem = new PopupMenu.PopupMenuItem('立即更新');
        refreshItem.connect('activate', () => this._refresh());
        this.menu.addMenuItem(refreshItem);

        const tuiItem = new PopupMenu.PopupMenuItem('開啟 TUI');
        tuiItem.connect('activate', () => this._openTui());
        this.menu.addMenuItem(tuiItem);

        const prefsItem = new PopupMenu.PopupMenuItem('設定');
        prefsItem.connect('activate', () => this._openPrefs());
        this.menu.addMenuItem(prefsItem);
    }

    // Group subtitle sitting above the rows that belong to it.
    _addHeading(text) {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        item.add_child(new St.Label({text, x_expand: true, style_class: 'aiub-header'}));
        this.menu.addMenuItem(item);
    }

    // A native, font-independent row: [name ........ value] / bar / reset.
    _addRow(key, name) {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        const vbox = new St.BoxLayout({
            vertical: true,
            x_expand: true,
            style_class: 'aiub-row',
        });

        const head = new St.BoxLayout({x_expand: true});
        const nameL = new St.Label({text: name, x_expand: true, style_class: 'aiub-row-name'});
        const valL = new St.Label({style_class: 'aiub-row-val'});
        head.add_child(nameL);
        head.add_child(valL);

        const barL = new St.Label({style_class: 'aiub-row-bar'});
        const resetL = new St.Label({style_class: 'aiub-row-reset'});

        vbox.add_child(head);
        vbox.add_child(barL);
        vbox.add_child(resetL);
        item.add_child(vbox);
        this.menu.addMenuItem(item);

        this._rows[key] = {item, nameL, valL, barL, resetL};
    }

    _colors() {
        const g = k => this._settings.get_string(k);
        return {
            low: g('color-low'),
            mid: g('color-mid'),
            high: g('color-high'),
            critical: g('color-critical'),
            empty: g('color-empty'),
        };
    }

    _restartTimer() {
        if (this._timer) {
            GLib.source_remove(this._timer);
            this._timer = 0;
        }
        const secs = Math.max(5, this._settings.get_int('refresh-interval'));
        this._timer = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, secs, () => {
            this._refresh();
            return GLib.SOURCE_CONTINUE;
        });
    }

    _refresh() {
        // Dropping the request while busy meant a vendor change *during* a
        // fetch never started one for the new vendor: the in-flight result for
        // the OLD vendor was applied and stayed on the panel until the next
        // timer tick. Remember that a refresh was asked for and run it as soon
        // as the current one settles.
        if (this._busy) {
            this._refreshPending = true;
            return;
        }
        this._busy = true;
        const token = ++this._refreshToken;

        const bin = resolveBinary(this._settings);
        // Capture the provider for this request so a superseded result can
        // never be painted into a replacement indicator.
        const vendor = this._fixedVendor;
        const argv = [bin, '--vendor', vendor, '--format', FORMAT];
        const cancellable = new Gio.Cancellable();
        this._refreshCancellable = cancellable;

        let proc;
        try {
            proc = new Gio.Subprocess({
                argv,
                flags: Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE,
            });
            proc.init(cancellable);
        } catch (e) {
            this._busy = false;
            this._refreshCancellable = null;
            this._refreshPending = false;
            this._setError(`無法執行「${bin}」`, String(e));
            return;
        }
        this._refreshProc = proc;

        let timedOut = false;
        const timeoutId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, REFRESH_TIMEOUT_SECS, () => {
            timedOut = true;
            if (this._refreshTimeoutId === timeoutId)
                this._refreshTimeoutId = 0;
            try {
                proc.force_exit();
            } catch (e) {}
            cancellable.cancel();
            if (this._refreshToken === token) {
                this._busy = false;
                this._setError('ai-usagebar 回應逾時', `等待超過 ${REFRESH_TIMEOUT_SECS} 秒`);
                // Do not strand a request that arrived while this one hung.
                if (this._refreshPending) {
                    this._refreshPending = false;
                    this._refresh();
                }
            }
            return GLib.SOURCE_REMOVE;
        });
        this._refreshTimeoutId = timeoutId;

        const cleanup = () => {
            if (this._refreshTimeoutId === timeoutId) {
                GLib.source_remove(timeoutId);
                this._refreshTimeoutId = 0;
            }
            if (this._refreshCancellable === cancellable)
                this._refreshCancellable = null;
            if (this._refreshProc === proc)
                this._refreshProc = null;
        };

        proc.communicate_utf8_async(null, cancellable, (p, res) => {
            const current = this._refreshToken === token;
            if (current)
                this._busy = false;
            try {
                const [, out, err] = p.communicate_utf8_finish(res);
                cleanup();
                if (timedOut)
                    return;
                // A superseded attempt must not paint the panel: its numbers
                // belong to whatever vendor was selected when it started.
                if (!current)
                    return;
                // Ignore a result if this indicator was repurposed while the
                // subprocess was running.
                if (this._fixedVendor !== vendor)
                    return;
                if ((!out || !out.trim()) && !p.get_successful()) {
                    this._setError('ai-usagebar 執行失敗', err || '');
                    return;
                }
                this._consume(out || '');
            } catch (e) {
                cleanup();
                if (current && !(e instanceof GLib.Error &&
                      e.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED)) && !timedOut)
                    this._setError('讀取輸出時發生錯誤', String(e));
            } finally {
                // Run whatever was requested while we were busy.
                if (current && this._refreshPending) {
                    this._refreshPending = false;
                    this._refresh();
                }
            }
        });
    }

    _consume(stdout) {
        let data;
        try {
            data = JSON.parse(stdout);
        } catch (e) {
            this._setError('輸出格式無效', stdout);
            return;
        }
        const raw = plainTextFromPango(data.text);
        const f = splitFormatOutput(raw);
        if (f.length <= FIELD.extraLimit) {
            // Loading… / ⚠ — show the binary's own text.
            this._data = null;
            this._label.clutter_text.set_markup(`<span foreground="${FG}">${esc(raw) || '…'}</span>`);
            return;
        }
        this._data = {
            plan: field(f[FIELD.plan]),
            stale: isStaleFormatOutput(f[FIELD.sentinel]),
            orBalance: field(f[FIELD.orBalance]),
            hasUsageWindows: hasUsageWindows(f[FIELD.vendorShort]),
            grouped: isGrouped(f[FIELD.sessionModel]),
            session: {pct: integer(f[FIELD.sessionPct]), reset: field(f[FIELD.sessionReset]),
                model: field(f[FIELD.sessionModel]),
                elapsed: markerElapsed(field(f[FIELD.sessionReset]), integer(f[FIELD.sessionElapsed]))},
            weekly: {pct: integer(f[FIELD.weeklyPct]), reset: field(f[FIELD.weeklyReset]),
                model: field(f[FIELD.weeklyModel]),
                elapsed: markerElapsed(field(f[FIELD.weeklyReset]), integer(f[FIELD.weeklyElapsed]))},
            // Per-model weekly bar: a non-empty scoped model is the presence
            // signal. A reset may be unavailable, which must not make us show
            // the unrelated legacy Sonnet window instead.
            sonnet: (() => {
                const scopedModel = field(f[FIELD.scopedModel]);
                if (scopedModel) {
                    const scopedPct = integer(f[FIELD.scopedPct]);
                    if (scopedPct != null && scopedPct >= 0 && scopedPct <= 100)
                        return {pct: scopedPct, reset: field(f[FIELD.scopedReset]) || '—',
                            model: scopedModel, label: scopedModel,
                            elapsed: markerElapsed(field(f[FIELD.scopedReset]), integer(f[FIELD.scopedElapsed]))};
                    // A scoped model with malformed data is unavailable; do
                    // not fall back to a potentially unrelated Sonnet window.
                    return {pct: null, reset: '—', model: scopedModel, label: scopedModel, elapsed: null};
                }
                return {pct: integer(f[FIELD.sonnetPct]), reset: field(f[FIELD.sonnetReset]),
                    model: '', label: 'Sonnet 專用額度', elapsed: null};
            })(),
            // A named extra window (model + reset) renders as a percentage bar;
            // without a name the slot stays a spent/limit money budget.
            extra: {pct: integer(f[FIELD.extraPct]), spent: field(f[FIELD.extraSpent]),
                limit: field(f[FIELD.extraLimit]), model: field(f[FIELD.extraModel]),
                reset: field(f[FIELD.extraReset]),
                elapsed: markerElapsed(field(f[FIELD.extraReset]), integer(f[FIELD.extraElapsed]))},
        };
        this._render();
    }

    // Redraw both the panel and the dropdown from cached data + settings.
    _render() {
        const d = this._data;
        if (!d)
            return;
        const colors = this._colors();
        this._renderPanel(d, colors);
        this._renderDropdown(d, colors);
    }

    _renderPanel(d, colors) {
        const w = Math.max(4, Math.min(20, this._settings.get_int('bar-width')));
        const showPct = this._settings.get_boolean('show-percent');
        const showBars = this._settings.get_boolean('show-bars');

        const seg = (tag, pct, valueText, elapsed, reset = '') => {
            const toks = [`<span foreground="${DIM}">${tag}</span>`];
            if (showPct)
                toks.push(`<span foreground="${colorForPct(pct, colors)}">${esc(valueText)}</span>`);
            if (showBars)
                toks.push(barMarkup(pct, w, colors, elapsed));
            if (!showPct && !showBars)
                toks.push(`<span foreground="${colorForPct(pct, colors)}">${esc(valueText)}</span>`);
            if (reset && reset !== '—')
                toks.push(`<span foreground="${DIM}">${esc(reset)}</span>`);
            return toks.join(' ');
        };

        const showSession = this._settings.get_boolean('show-session');
        const showWeekly = this._settings.get_boolean('show-weekly');
        const parts = [];

        // OpenRouter exposes its real remaining credit via {or_balance}.
        if (this._fixedVendor === 'openrouter' && d.orBalance) {
            this._label.clutter_text.set_markup(
                `<span foreground="${DIM}">◈</span> ` +
                `<span foreground="${FG}">${esc(d.orBalance)}</span>`
            );
            return;
        }

        if (d.grouped) {
            // Antigravity currently exposes weekly-only quota pools.
            // G = Gemini, C = Claude & GPT OSS. The weekly visibility toggle
            // controls both because neither pool has a 5-hour panel segment.
            if (this._fixedVendor === 'antigravity') {
                const groupedSeg = (tag, window) => {
                    const value = seg(tag, window.pct, `${window.pct}%`, window.elapsed);
                    const reset = field(window.reset);
                    return reset && reset !== '—'
                        ? `${value} <span foreground="${DIM}">${esc(reset)}</span>`
                        : value;
                };
                if (showWeekly && d.weekly.pct != null)
                    parts.push(groupedSeg('G', d.weekly));
                if (showWeekly && d.extra.pct != null)
                    parts.push(groupedSeg('C', d.extra));
            }
        } else {
            if (d.hasUsageWindows && showSession && d.session.pct != null)
                parts.push(seg('5h', d.session.pct, `${d.session.pct}%`, d.session.elapsed, d.session.reset));
            if (d.hasUsageWindows && showWeekly && d.weekly.pct != null)
                parts.push(seg('7d', d.weekly.pct, `${d.weekly.pct}%`, d.weekly.elapsed, d.weekly.reset));
            if (this._settings.get_boolean('show-extra') &&
                d.extra.pct != null && d.extra.spent && d.extra.limit)
                parts.push(seg('ex', d.extra.pct, d.extra.spent, null)); // $ budget → no meta
        }

        const gap = `<span foreground="${DIM}">   </span>`;
        this._label.clutter_text.set_markup(parts.join(gap) || ' ');
    }

    _renderDropdown(d, colors) {
        // Switching vendors can flip the layout; rebuild once when it does.
        if (!!d.grouped !== !!this._grouped)
            this._buildMenu(d.grouped);

        this._planLabel.text = d.plan || 'AI Usage';

        const upd = (key, pct, valueText, reset, visible, elapsed) => {
            const r = this._rows[key];
            r.item.visible = visible;
            if (!visible)
                return;
            r.valL.text = valueText;
            r.barL.clutter_text.set_markup(barMarkup(pct ?? 0, 18, colors, elapsed));
            if (reset) {
                r.resetL.text = `↺ ${reset} 後重設`;
                r.resetL.visible = true;
            } else {
                r.resetL.visible = false;
            }
        };

        // Under a group heading the row is named by its pool, not by the window.
        this._rows.session.nameL.text = d.session.model || '5 小時額度';
        this._rows.weekly.nameL.text = d.weekly.model || '每週額度';
        upd('session', d.session.pct, `${d.session.pct ?? 0}%`, d.session.reset,
            d.hasUsageWindows && d.session.pct != null, d.session.elapsed);
        upd('weekly', d.weekly.pct, `${d.weekly.pct ?? 0}%`, d.weekly.reset,
            d.hasUsageWindows && d.weekly.pct != null, d.weekly.elapsed);
        this._rows.sonnet.nameL.text = d.sonnet.label || 'Sonnet 專用額度';
        upd('sonnet', d.sonnet.pct, `${d.sonnet.pct ?? 0}%`, d.sonnet.reset, d.sonnet.pct != null, d.sonnet.elapsed);
        if (d.extra.model) {
            // Named quota window (e.g. Antigravity's "Claude & GPT OSS (weekly)").
            this._rows.extra.nameL.text = d.extra.model;
            upd('extra', d.extra.pct, `${d.extra.pct}%`, d.extra.reset || '—',
                d.extra.pct != null, d.extra.elapsed);
        } else {
            this._rows.extra.nameL.text = '額外使用量';
            upd('extra', d.extra.pct, `${d.extra.spent} / ${d.extra.limit}`, null,
                d.extra.pct != null && !!d.extra.spent && !!d.extra.limit, null); // $ budget → no meta
        }
    }

    _setError(short, detail) {
        this._data = null;
        this._label.clutter_text.set_markup(`<span foreground="${RED}">⚠ ai</span>`);
        const msg = detail ? `${short}\n${esc(detail).slice(0, 300)}` : short;
        this._planLabel.clutter_text.set_markup(`<span foreground="${FG}">${esc(msg)}</span>`);
        for (const r of Object.values(this._rows))
            r.item.visible = false;
    }

    _openTui() {
        const tui = GLib.find_program_in_path('ai-usagebar-tui') ||
            `${GLib.get_home_dir()}/.cargo/bin/ai-usagebar-tui`;
        const candidates = [
            ['kgx', '--', tui],
            ['gnome-terminal', '--', tui],
            ['xterm', '-e', tui],
        ];
        for (const argv of candidates) {
            if (!GLib.find_program_in_path(argv[0]))
                continue;
            try {
                Gio.Subprocess.new(argv, Gio.SubprocessFlags.NONE);
                return;
            } catch (e) {
                // try the next terminal
            }
        }
        Main.notify('AI Usage Bar', '找不到終端機（kgx / gnome-terminal / xterm）。');
    }

    destroy() {
        if (this._timer) {
            GLib.source_remove(this._timer);
            this._timer = 0;
        }
        if (this._refreshTimeoutId) {
            GLib.source_remove(this._refreshTimeoutId);
            this._refreshTimeoutId = 0;
        }
        if (this._refreshCancellable)
            this._refreshCancellable.cancel();
        if (this._refreshProc) {
            try {
                this._refreshProc.force_exit();
            } catch (e) {}
            this._refreshProc = null;
        }
        for (const id of this._viewIds ?? [])
            this._settings.disconnect(id);
        for (const id of this._sourceIds ?? [])
            this._settings.disconnect(id);
        if (this._intervalId)
            this._settings.disconnect(this._intervalId);
        this._viewIds = this._sourceIds = null;
        this._intervalId = 0;
        super.destroy();
    }
});

export default class AiUsageBarExtension extends Extension {
    enable() {
        this._settings = this.getSettings();
        this._place();
        this._placeIds = [
            this._settings.connect('changed::panel-box', () => this._place()),
            this._settings.connect('changed::panel-index', () => this._place()),
        ];
    }

    _place() {
        for (const entry of this._indicators ?? []) {
            entry.indicator.destroy();
            delete Main.panel.statusArea[entry.role];
        }
        this._indicators = [];
        const box = this._settings.get_string('panel-box') || 'right';
        const index = Math.max(0, this._settings.get_int('panel-index'));

        const vendors = ['openai', 'openrouter', 'antigravity'];

        vendors.forEach((vendor, offset) => {
            const indicator = new Indicator(
                this._settings,
                () => this.openPreferences(),
                vendor
            );

            const role = `${ROLE}-${vendor}`;

            Main.panel.addToStatusArea(
                role,
                indicator,
                index + offset,
                box
            );

            this._indicators.push({role, indicator});
        });
    }

    disable() {
        for (const id of this._placeIds ?? [])
            this._settings.disconnect(id);
        this._placeIds = null;
        for (const entry of this._indicators ?? []) {
            entry.indicator.destroy();
            delete Main.panel.statusArea[entry.role];
        }
        this._indicators = [];
        this._settings = null;
    }
}

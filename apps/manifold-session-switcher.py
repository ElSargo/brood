#!/usr/bin/env python3
import argparse
import os
import sys

import dbus
import dbus.mainloop.glib
import gi

dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)

force_gtk3 = os.environ.get("MANIFOLD_SESSION_SWITCHER_FORCE_GTK3") == "1"
disable_adwaita = os.environ.get("MANIFOLD_SESSION_SWITCHER_DISABLE_ADWAITA") == "1"

try:
    if force_gtk3:
        raise ValueError("GTK3 fallback forced")
    gi.require_version("Gdk", "4.0")
    gi.require_version("Gtk", "4.0")
    from gi.repository import Gdk, Gio, GLib, Gtk, Pango

    try:
        if disable_adwaita:
            raise ValueError("libadwaita disabled")
        gi.require_version("Adw", "1")
        from gi.repository import Adw
    except (ImportError, ValueError):
        Adw = None

    GTK4 = True
except (ImportError, ValueError):
    Adw = None
    gi.require_version("Gdk", "3.0")
    gi.require_version("Gtk", "3.0")
    from gi.repository import Gdk, Gio, GLib, Gtk, Pango

    GTK4 = False


APP_ID = "org.manifold.SessionSwitcher"
DEFAULT_DESTINATION = "org.manifold.Session"
DISCLOSURE_COLUMN_WIDTH = 28
PASSTHROUGH_STATUS_CHANNEL = "workspace.input.passthrough"
NOTIFICATIONS_DESTINATION = "org.freedesktop.Notifications"
NOTIFICATIONS_PATH = "/org/freedesktop/Notifications"
NOTIFICATION_ACTION_DEFAULT = "default"


def metadata(**items):
    return dbus.Dictionary({key: str(value) for key, value in items.items()}, signature="ss")


def write_status(path, **items):
    if not path:
        return
    with open(path, "a", encoding="utf-8") as handle:
        for key, value in items.items():
            handle.write(f"{key}={value}\n")


class ManifoldBus:
    def __init__(self, address=None, destination=None):
        self.address = address or os.environ.get("MANIFOLDD_DBUS_ADDRESS")
        self.destination = (
            destination
            or os.environ.get("MANIFOLDD_DBUS_DESTINATION")
            or DEFAULT_DESTINATION
        )
        self.bus = (
            dbus.bus.BusConnection(self.address) if self.address else dbus.SessionBus()
        )
        self.manager = dbus.Interface(
            self.bus.get_object(self.destination, "/org/manifold/Manager"),
            "org.manifold.Manager",
        )
        self.control = dbus.Interface(
            self.bus.get_object(self.destination, "/org/manifold/Session"),
            "org.manifold.Session.Control",
        )
        self.session = dbus.Interface(
            self.bus.get_object(self.destination, "/org/manifold/Session"),
            "org.manifold.Session",
        )
        self.extension = dbus.Interface(
            self.bus.get_object(self.destination, "/org/manifold/Extension"),
            "org.manifold.Extension",
        )
        self.compositor = dbus.Interface(
            self.bus.get_object(self.destination, "/org/manifold/Compositor"),
            "org.manifold.Compositor",
        )
        self.agent = dbus.Interface(
            self.bus.get_object(self.destination, "/org/manifold/Agent"),
            "org.manifold.Agent",
        )
        self.status = dbus.Interface(
            self.bus.get_object(self.destination, "/org/manifold/Status"),
            "org.manifold.Status",
        )
        self.notifications = dbus.Interface(
            self.bus.get_object(NOTIFICATIONS_DESTINATION, NOTIFICATIONS_PATH),
            NOTIFICATIONS_DESTINATION,
        )

    def list_sessions(self):
        rows = []
        for session_id, name, active, windows in self.manager.ListSessions():
            rows.append(
                {
                    "id": int(session_id),
                    "name": str(name),
                    "active": bool(active),
                    "windows": int(windows),
                }
            )
        return rows

    def list_agents(self):
        rows = []
        for raw in self.agent.ListAgents():
            (
                agent_id,
                kind,
                status,
                sender,
                session,
                attention,
                unread,
                notifications,
                last_title,
                last_body,
                metadata,
            ) = raw
            rows.append(
                {
                    "id": str(agent_id),
                    "kind": str(kind),
                    "status": str(status),
                    "sender": str(sender),
                    "session": int(session),
                    "attention": bool(attention),
                    "unread": int(unread),
                    "notifications": int(notifications),
                    "last_title": str(last_title),
                    "last_body": str(last_body),
                    "metadata": {
                        str(key): str(value) for key, value in dict(metadata).items()
                    },
                }
            )
        return rows

    def list_status(self):
        rows = []
        for channel, state, label, publisher, metadata in self.status.ListStatus():
            rows.append(
                {
                    "channel": str(channel),
                    "state": str(state),
                    "label": str(label),
                    "publisher": str(publisher),
                    "metadata": {
                        str(key): str(value) for key, value in dict(metadata).items()
                    },
                }
            )
        return rows

    def set_passthrough_mode(self, enabled):
        return bool(self.status.SetPassthroughMode(bool(enabled)))

    def activate(self, session_id):
        return bool(self.session.ActivateSurfaceSession(dbus.UInt64(session_id)))

    def create(self, name=None):
        argv = ["surface-session", "new"]
        if name:
            argv.append(str(name))
        response = str(self.control.Command(dbus.Array(argv, signature="s"), False))
        if not response.startswith("OK "):
            raise RuntimeError(response.strip())
        return int(self.manager.ActiveSession())

    def focus_app_id(self, app_id):
        return str(
            self.control.Command(
                dbus.Array(["focus-app-id", str(app_id)], signature="s"),
                False,
            )
        )

    def focus_window(self, identity):
        argv = ["focus-window"]
        for key in ("pane_id", "pid", "app_id", "title"):
            value = identity.get(key, "")
            if value:
                argv.append(f"{key}={value}")
        if len(argv) == 1:
            return "OK focus-window changed=false missing-identity"
        return str(self.control.Command(dbus.Array(argv, signature="s"), False))

    def clear_agent_attention(self, agent_id):
        return bool(self.agent.ClearAgentAttention(str(agent_id)))

    def should_notify_agent_desktop(self, agent_id):
        return bool(self.agent.ShouldNotifyAgentDesktop(str(agent_id)))

    def notify_agent_finished(self, summary, body, timeout_ms):
        notification_id = self.notifications.Notify(
            APP_ID,
            dbus.UInt32(0),
            "",
            summary,
            body,
            dbus.Array([NOTIFICATION_ACTION_DEFAULT, "Open"], signature="s"),
            dbus.Dictionary({}, signature="sv"),
            dbus.Int32(timeout_ms),
        )
        return int(notification_id)

    def focus_presentation_window(self, activation_token=""):
        if activation_token:
            return bool(
                self.compositor.FocusPresentationWindowWithActivationToken(
                    str(activation_token)
                )
            )
        return bool(self.compositor.FocusPresentationWindow())

    def register_sidebar(self, width):
        extension_id = self.extension.RegisterExtension(
            "session-switcher",
            metadata(name="manifold-session-switcher", toolkit="gtk-libadwaita"),
        )
        window_id = self.compositor.RegisterWindow(
            extension_id,
            metadata(pid=os.getpid()),
            "session-switcher",
        )
        membership_grant = self.compositor.RequestPresentationMembership(
            window_id,
            metadata(audience="Presentation"),
        )
        placement_grant = self.compositor.RequestPlacement(
            window_id,
            metadata(
                mode="sidebar",
                edge="left",
                audience="Presentation",
                width=width,
                visible="true",
            ),
        )
        try:
            shortcut_grant = self.compositor.RequestShortcut(
                extension_id,
                "Ctrl+Alt+M",
                "Presentation",
                "Focus session switcher",
            )
        except dbus.DBusException as err:
            shortcut_grant = f"ungranted:{err.get_dbus_name()}"
        return extension_id, window_id, membership_grant, placement_grant, shortcut_grant


class SessionSwitcher:
    def __init__(self, args):
        self.args = args
        self.bus = ManifoldBus(args.dbus_address, args.dbus_name)
        self.sessions = []
        self.agents = []
        self.status_items = {}
        self.filtered_sessions = []
        self.expanded_sessions = set()
        self.session_order = []
        self.session_rows = {}
        self.agent_done_state = {}
        self.agent_done_state_initialized = False
        self.notification_agents = {}
        self.notification_tokens = {}
        self.activated_notifications = set()
        self.animation_tick = 0
        self.registered = False
        self.last_error = None

    def run(self):
        if not GTK4:
            self.window = self.make_window(None)
            self.window.connect("destroy", Gtk.main_quit)
            self.build_ui()
            self.setup_notifications()
            self.refresh_sessions()
            self.refresh_statuses()
            self.present_window()
            GLib.timeout_add(250, self.register_with_manifold)
            GLib.timeout_add(self.args.refresh_ms, self.refresh_sessions)
            GLib.timeout_add(self.args.refresh_ms, self.refresh_statuses)
            GLib.timeout_add(450, self.animate_working_agents)
            Gtk.main()
            return 0

        if Adw is not None:
            app = Adw.Application(
                application_id=APP_ID,
                flags=Gio.ApplicationFlags.NON_UNIQUE,
            )
        else:
            app = Gtk.Application(
                application_id=APP_ID,
                flags=Gio.ApplicationFlags.NON_UNIQUE,
            )
        app.connect("activate", self.on_activate)
        return app.run([])

    def on_activate(self, app):
        self.window = self.make_window(app)
        self.build_ui()
        self.setup_notifications()
        self.refresh_sessions()
        self.refresh_statuses()
        self.present_window()
        GLib.timeout_add(250, self.register_with_manifold)
        GLib.timeout_add(self.args.refresh_ms, self.refresh_sessions)
        GLib.timeout_add(self.args.refresh_ms, self.refresh_statuses)
        GLib.timeout_add(450, self.animate_working_agents)

    def make_window(self, app):
        if GTK4:
            window = Gtk.ApplicationWindow(application=app)
            window.set_default_size(self.args.width, 720)
        else:
            window = Gtk.Window()
            window.set_default_size(self.args.width, 720)
        window.set_title("Manifold Sessions")
        return window

    def setup_notifications(self):
        self.bus.bus.add_signal_receiver(
            self.handle_status_changed,
            signal_name="StatusChanged",
            dbus_interface="org.manifold.Status",
            path="/org/manifold/Status",
        )
        self.bus.bus.add_signal_receiver(
            self.handle_status_cleared,
            signal_name="StatusCleared",
            dbus_interface="org.manifold.Status",
            path="/org/manifold/Status",
        )
        self.bus.bus.add_signal_receiver(
            self.handle_notification_action,
            signal_name="ActionInvoked",
            dbus_interface=NOTIFICATIONS_DESTINATION,
            path=NOTIFICATIONS_PATH,
        )
        self.bus.bus.add_signal_receiver(
            self.handle_notification_activation_token,
            signal_name="ActivationToken",
            dbus_interface=NOTIFICATIONS_DESTINATION,
            path=NOTIFICATIONS_PATH,
        )
        self.bus.bus.add_signal_receiver(
            self.handle_notification_closed,
            signal_name="NotificationClosed",
            dbus_interface=NOTIFICATIONS_DESTINATION,
            path=NOTIFICATIONS_PATH,
        )

    def present_window(self):
        if GTK4:
            self.window.present()
        else:
            self.window.show_all()

    def build_ui(self):
        self.install_css()
        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        outer.set_margin_top(10)
        outer.set_margin_bottom(10)
        outer.set_margin_start(10)
        outer.set_margin_end(10)

        create_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        self.create_entry = Gtk.Entry()
        self.create_entry.set_placeholder_text("New session")
        self.create_entry.connect("activate", self.create_session)
        if GTK4:
            create_button = Gtk.Button.new_from_icon_name("list-add-symbolic")
        else:
            create_button = Gtk.Button.new_from_icon_name(
                "list-add-symbolic", Gtk.IconSize.BUTTON
            )
        create_button.set_tooltip_text("Create session")
        create_button.connect("clicked", self.create_session)
        self.pack(create_row, self.create_entry, expand=True)
        self.pack(create_row, create_button, expand=False)
        self.pack(outer, create_row, expand=False)

        self.search_entry = Gtk.Entry()
        self.search_entry.set_placeholder_text("Search sessions")
        self.search_entry.connect("changed", self.filter_sessions)
        self.pack(outer, self.search_entry, expand=False)

        if GTK4:
            self.listbox = Gtk.ListBox()
            self.listbox.set_selection_mode(Gtk.SelectionMode.NONE)
            self.listbox.connect("row-activated", self.activate_row)
        else:
            self.listbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        scroller = Gtk.ScrolledWindow()
        if GTK4:
            scroller.set_child(self.listbox)
        else:
            scroller.add(self.listbox)
        self.pack(outer, scroller, expand=True)

        self.status_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        self.status_box.set_hexpand(True)
        self.passthrough_button = Gtk.Button(label="Passthrough Off")
        self.passthrough_button.get_style_context().add_class("status-indicator")
        self.passthrough_button.set_tooltip_text("Toggle passthrough")
        self.passthrough_button.connect("clicked", self.toggle_passthrough_mode)
        self.passthrough_button.set_hexpand(True)
        self.pack(self.status_box, self.passthrough_button, expand=True)
        self.pack(outer, self.status_box, expand=False)

        if GTK4:
            window_legacy = Gtk.EventControllerLegacy.new()
            window_legacy.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
            window_legacy.connect("event", self.handle_legacy_event)
            self.window.add_controller(window_legacy)

            click = Gtk.GestureClick()
            click.set_button(0)
            click.connect("pressed", self.activate_session_from_panel_press)
            click.connect("released", self.activate_session_from_panel_click)
            outer.add_controller(click)
            if hasattr(self.window, "set_content"):
                self.window.set_content(outer)
            else:
                self.window.set_child(outer)
        else:
            outer.add_events(Gdk.EventMask.BUTTON_RELEASE_MASK)
            outer.connect("button-release-event", self.activate_session_from_gtk3_click)
            self.window.add_events(Gdk.EventMask.BUTTON_RELEASE_MASK)
            self.window.connect("button-release-event", self.activate_session_from_gtk3_click)
            self.window.add(outer)

    def install_css(self):
        css = Gtk.CssProvider()
        css.load_from_data(
            b"""
            button.session-select,
            button.dropdown-toggle {
              background: transparent;
              border: 1px solid transparent;
              box-shadow: none;
              text-shadow: none;
              padding: 3px 5px;
              min-height: 28px;
              min-width: 28px;
            }
            button.session-select.active-session {
              border-color: rgba(90, 150, 190, 0.85);
              background: rgba(90, 150, 190, 0.10);
            }
            button.agent-select {
              padding: 2px 6px;
              min-height: 24px;
            }
            button.session-select:hover,
            button.dropdown-toggle:hover {
              background: rgba(120, 120, 120, 0.14);
            }
            .agent-dot-working {
              opacity: 0.30;
            }
            .agent-dot-working.pulse {
              opacity: 1.0;
            }
            button.status-indicator {
              background: rgba(120, 120, 120, 0.10);
              border: 1px solid rgba(120, 120, 120, 0.45);
              box-shadow: none;
              text-shadow: none;
              padding: 6px 8px;
              min-height: 32px;
            }
            button.status-indicator.passthrough-active {
              background: rgba(210, 135, 40, 0.16);
              border-color: rgba(210, 135, 40, 0.85);
            }
            """
        )
        if GTK4:
            display = Gdk.Display.get_default()
            if display is not None:
                Gtk.StyleContext.add_provider_for_display(
                    display, css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
                )
        else:
            screen = Gdk.Screen.get_default()
            if screen is not None:
                Gtk.StyleContext.add_provider_for_screen(
                    screen, css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
                )

    def pack(self, box, child, expand):
        if GTK4:
            if expand:
                child.set_vexpand(True)
                child.set_hexpand(True)
            box.append(child)
        else:
            box.pack_start(child, expand, expand, 0)

    def register_with_manifold(self):
        try:
            extension_id, window_id, membership, placement, shortcut = (
                self.bus.register_sidebar(self.args.width)
            )
            self.registered = True
            write_status(
                self.args.ready_file,
                ready="true",
                pid=os.getpid(),
                extension_id=extension_id,
                window_id=window_id,
                membership_grant=membership,
                placement_grant=placement,
                shortcut_grant=shortcut,
            )
        except Exception as err:
            write_status(self.args.ready_file, ready="false", error=err)
        return False

    def refresh_sessions(self):
        try:
            sessions = self.bus.list_sessions()
            agents = self.bus.list_agents()
            self.animation_tick = (self.animation_tick + 1) % 2
            self.last_error = None
            live_session_ids = {session["id"] for session in sessions}
            self.session_order = [
                session_id
                for session_id in self.session_order
                if session_id in live_session_ids
            ]
            for session in sessions:
                if session["id"] not in self.session_order:
                    self.session_order.append(session["id"])
            self.update_finished_agents(agents)
            self.expanded_sessions.intersection_update(live_session_ids)
            if sessions != self.sessions or agents != self.agents:
                self.sessions = sessions
                self.agents = agents
                self.filter_sessions()
        except Exception as err:
            self.last_error = str(err)
            write_status(self.args.event_file, event="refresh-error", error=err)
        return True

    def refresh_statuses(self):
        try:
            self.status_items = {
                item["channel"]: item for item in self.bus.list_status()
            }
            self.render_status_indicators()
        except Exception as err:
            write_status(self.args.event_file, event="status-refresh-error", error=err)
        return True

    def handle_status_changed(self, channel, state, label, publisher, metadata):
        self.status_items[str(channel)] = {
            "channel": str(channel),
            "state": str(state),
            "label": str(label),
            "publisher": str(publisher),
            "metadata": {str(key): str(value) for key, value in dict(metadata).items()},
        }
        GLib.idle_add(self.render_status_indicators)

    def handle_status_cleared(self, channel, _publisher):
        self.status_items.pop(str(channel), None)
        GLib.idle_add(self.render_status_indicators)

    def render_status_indicators(self):
        item = self.status_items.get(PASSTHROUGH_STATUS_CHANNEL)
        passthrough_active = item is not None and item["state"] == "active"
        self.passthrough_button.set_label(
            "Passthrough On" if passthrough_active else "Passthrough Off"
        )
        context = self.passthrough_button.get_style_context()
        if passthrough_active:
            context.add_class("passthrough-active")
        else:
            context.remove_class("passthrough-active")
        return False

    def toggle_passthrough_mode(self, *_args):
        try:
            item = self.status_items.get(PASSTHROUGH_STATUS_CHANNEL)
            passthrough_active = item is not None and item["state"] == "active"
            next_enabled = not passthrough_active
            changed = self.bus.set_passthrough_mode(next_enabled)
            write_status(
                self.args.event_file,
                event="toggle-passthrough",
                enabled=str(next_enabled).lower(),
                changed=str(changed).lower(),
            )
            self.refresh_statuses()
        except Exception as err:
            write_status(self.args.event_file, event="toggle-passthrough-error", error=err)

    def update_finished_agents(self, agents):
        current_done_state = {agent["id"]: self.agent_is_done(agent) for agent in agents}
        if not self.agent_done_state_initialized:
            self.agent_done_state = current_done_state
            self.agent_done_state_initialized = True
            return

        for agent in agents:
            agent_id = agent["id"]
            done = current_done_state.get(agent_id, False)
            if not done or self.agent_done_state.get(agent_id, False):
                continue
            self.expanded_sessions.add(agent["session"])
            self.maybe_notify_agent_finished(agent)
        self.agent_done_state = current_done_state

    def maybe_notify_agent_finished(self, agent):
        try:
            should_notify = self.bus.should_notify_agent_desktop(agent["id"])
        except Exception as err:
            should_notify = True
            write_status(
                self.args.event_file,
                event="notify-condition-error",
                agent=agent["id"],
                error=err,
            )
        if not should_notify:
            write_status(
                self.args.event_file,
                event="notify-suppressed",
                agent=agent["id"],
            )
            return

        try:
            notification_id = self.bus.notify_agent_finished(
                "Agent finished",
                self.agent_label(agent),
                self.args.notification_timeout_ms,
            )
            self.notification_agents[notification_id] = agent
            write_status(
                self.args.event_file,
                event="notify-finished",
                agent=agent["id"],
                notification=notification_id,
            )
        except Exception as err:
            write_status(
                self.args.event_file,
                event="notify-error",
                agent=agent["id"],
                error=err,
            )

    def filter_sessions(self, *_args):
        query = self.search_entry.get_text().strip().lower()
        if query:
            self.filtered_sessions = [
                session
                for session in self.sessions
                if query in session["name"].lower() or query in str(session["id"])
            ]
        else:
            self.filtered_sessions = list(self.sessions)
        order = {session_id: index for index, session_id in enumerate(self.session_order)}
        self.filtered_sessions.sort(
            key=lambda session: (order.get(session["id"], len(order)), session["id"])
        )
        self.render_sessions()

    def render_sessions(self):
        self.agents_by_session = {}
        for agent in self.agents:
            self.agents_by_session.setdefault(agent["session"], []).append(agent)
        visible_session_ids = {session["id"] for session in self.filtered_sessions}
        live_session_ids = {session["id"] for session in self.sessions}

        for session in self.filtered_sessions:
            self.add_session_row(session)
            self.session_rows[session["id"]]["root"].set_visible(True)

        for session_id, widgets in list(self.session_rows.items()):
            if session_id not in live_session_ids:
                self.listbox.remove(widgets["root"])
                del self.session_rows[session_id]
            elif session_id not in visible_session_ids:
                widgets["root"].set_visible(False)

        write_status(
            self.args.event_file,
            event="render",
            sessions=len(self.sessions),
            agents=len(self.agents),
            rows=len(self.filtered_sessions),
        )

    def clear_listbox(self):
        if GTK4:
            child = self.listbox.get_first_child()
            while child is not None:
                next_child = child.get_next_sibling()
                self.listbox.remove(child)
                child = next_child
        else:
            for child in self.listbox.get_children():
                self.listbox.remove(child)

    def add_session_row(self, session):
        label = f"{session['name']}  {session['windows']}w"
        agents = self.agents_by_session.get(session["id"], [])
        if agents:
            label = f"{label}  {len(agents)}a"

        if session["id"] in self.session_rows:
            widgets = self.session_rows[session["id"]]
            widgets["label"].set_text(label)
            button_context = widgets["button"].get_style_context()
            if session["active"]:
                button_context.add_class("active-session")
            else:
                button_context.remove_class("active-session")
            widgets["toggle"].set_label(
                "v" if session["id"] in self.expanded_sessions else ">"
            )
            widgets["toggle"].set_sensitive(bool(agents))
            widgets["agent_box"].set_visible(session["id"] in self.expanded_sessions)
            agent_signature = tuple(
                (
                    agent["id"],
                    agent["status"],
                    agent["attention"],
                    agent["last_title"],
                    agent["last_body"],
                    tuple(sorted(agent["metadata"].items())),
                )
                for agent in agents
            )
            if agent_signature != widgets["agent_signature"]:
                self.replace_agent_items(widgets["agent_box"], agents)
                widgets["agent_signature"] = agent_signature
            return

        row_shell = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        row_shell.set_hexpand(True)
        row_shell.set_opacity(0.0)
        toggle_label = "v" if session["id"] in self.expanded_sessions else ">"
        toggle_button = Gtk.Button(label=toggle_label)
        toggle_button.get_style_context().add_class("dropdown-toggle")
        toggle_button.set_sensitive(bool(agents))
        toggle_button.set_hexpand(False)
        toggle_button.set_vexpand(False)
        toggle_button.set_valign(Gtk.Align.START)
        toggle_button.set_size_request(DISCLOSURE_COLUMN_WIDTH, -1)
        self.pack(row_shell, toggle_button, expand=False)

        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=3)
        content.set_hexpand(True)
        content.set_vexpand(False)

        session_button = Gtk.Button()
        session_button.get_style_context().add_class("session-select")
        if session["active"]:
            session_button.get_style_context().add_class("active-session")
        session_button.set_size_request(-1, 28)
        session_button.set_hexpand(True)
        session_button.connect("clicked", self.activate_session, session["id"])
        session_label = Gtk.Label(label=label)
        session_label.set_xalign(0)
        session_label.set_hexpand(True)
        session_label.set_ellipsize(Pango.EllipsizeMode.END)
        if GTK4:
            session_button.set_child(session_label)
        else:
            session_button.add(session_label)
        self.pack(content, session_button, expand=False)

        agent_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=3)
        agent_box.set_hexpand(True)
        self.replace_agent_items(agent_box, agents)
        agent_box.set_visible(session["id"] in self.expanded_sessions)
        toggle_button.connect(
            "clicked",
            self.toggle_session_agents,
            session["id"],
            agent_box,
            toggle_button,
        )

        self.pack(content, agent_box, expand=False)
        self.pack(row_shell, content, expand=True)

        if GTK4:
            row = Gtk.ListBoxRow()
            row.session_id = session["id"]
            row.set_activatable(False)
            row.set_child(row_shell)
            self.listbox.append(row)
            root = row
        else:
            self.listbox.pack_start(row_shell, False, False, 0)
            row_shell.show_all()
            agent_box.set_visible(session["id"] in self.expanded_sessions)
            root = row_shell

        self.fade_in(row_shell)
        self.session_rows[session["id"]] = {
            "root": root,
            "button": session_button,
            "label": session_label,
            "toggle": toggle_button,
            "agent_box": agent_box,
            "agent_signature": tuple(
                (
                    agent["id"],
                    agent["status"],
                    agent["attention"],
                    agent["last_title"],
                    agent["last_body"],
                    tuple(sorted(agent["metadata"].items())),
                )
                for agent in agents
            ),
        }

    def replace_agent_items(self, agent_box, agents):
        if GTK4:
            child = agent_box.get_first_child()
            while child is not None:
                next_child = child.get_next_sibling()
                agent_box.remove(child)
                child = next_child
        else:
            for child in agent_box.get_children():
                agent_box.remove(child)
        if agents:
            for agent in agents:
                self.pack(agent_box, self.make_agent_button(agent), expand=False)
        else:
            empty = Gtk.Label(label="No agents")
            empty.set_xalign(0)
            empty.set_margin_start(0)
            empty.set_margin_bottom(4)
            self.pack(agent_box, empty, expand=False)
        if not GTK4:
            agent_box.show_all()

    def fade_in(self, widget, step=0):
        opacity = min(1.0, 0.20 + step * 0.20)
        widget.set_opacity(opacity)
        if opacity >= 1.0:
            return False
        GLib.timeout_add(16, self.fade_in, widget, step + 1)
        return False

    def fade_out_then_hide(self, widget, step=0):
        opacity = max(0.0, 1.0 - step * 0.25)
        widget.set_opacity(opacity)
        if opacity <= 0.0:
            widget.set_visible(False)
            widget.set_opacity(1.0)
            return False
        GLib.timeout_add(16, self.fade_out_then_hide, widget, step + 1)
        return False

    def animate_working_agents(self):
        self.animation_tick = (self.animation_tick + 1) % 2
        for widgets in self.session_rows.values():
            agent_box = widgets.get("agent_box")
            if agent_box is None:
                continue
            if GTK4:
                child = agent_box.get_first_child()
                while child is not None:
                    self.update_agent_dot_pulse(child)
                    child = child.get_next_sibling()
            else:
                for child in agent_box.get_children():
                    self.update_agent_dot_pulse(child)
        return True

    def update_agent_dot_pulse(self, widget):
        dot = getattr(widget, "agent_dot", None)
        if dot is None and hasattr(widget, "get_child"):
            child = widget.get_child()
            dot = getattr(child, "agent_dot", None)
        if dot is None:
            return
        context = dot.get_style_context()
        if self.animation_tick:
            context.add_class("pulse")
        else:
            context.remove_class("pulse")

    def toggle_session_agents(self, _button, session_id, agent_box, toggle_button):
        if session_id in self.expanded_sessions:
            self.expanded_sessions.remove(session_id)
            self.fade_out_then_hide(agent_box)
            toggle_button.set_label(">")
        else:
            self.expanded_sessions.add(session_id)
            agent_box.set_visible(True)
            agent_box.set_opacity(0.0)
            self.fade_in(agent_box)
            toggle_button.set_label("v")

    def make_agent_button(self, agent):
        button = Gtk.Button()
        button.get_style_context().add_class("agent-select")
        button.set_tooltip_text(agent["id"])
        button.connect("clicked", self.activate_agent, agent)
        button.set_hexpand(True)

        row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        row.set_hexpand(True)
        row.set_margin_start(0)
        row.set_margin_end(0)
        row.set_margin_top(0)
        row.set_margin_bottom(0)

        dot = Gtk.Label()
        color = "#2ea043" if self.agent_is_done(agent) else "#1f6feb"
        dot.set_markup(f"<span foreground='{color}'>●</span>")
        if not self.agent_is_done(agent):
            dot.get_style_context().add_class("agent-dot-working")
            if self.animation_tick:
                dot.get_style_context().add_class("pulse")
        self.pack(row, dot, expand=False)
        row.agent_dot = dot

        text = self.agent_label(agent)
        label = Gtk.Label(label=text)
        label.set_xalign(0)
        label.set_hexpand(True)
        label.set_ellipsize(Pango.EllipsizeMode.END)
        self.pack(row, label, expand=True)

        if GTK4:
            button.set_child(row)
        else:
            button.add(row)
        return button

    def agent_is_done(self, agent):
        return agent["status"].lower() in {
            "done",
            "complete",
            "completed",
            "finished",
            "idle",
        }

    def agent_label(self, agent):
        metadata = agent["metadata"]
        detail = (
            metadata.get("first_user_message")
            or metadata.get("prompt")
            or metadata.get("userPrompt")
            or metadata.get("last_event")
            or metadata.get("task")
            or metadata.get("cwd")
            or metadata.get("terminal")
            or ""
        )
        if detail:
            return self.clean_agent_detail(detail)
        return "Agent"

    def clean_agent_detail(self, detail):
        detail = " ".join(str(detail).split())
        if detail.startswith("{") or detail.startswith("["):
            return "event"
        if len(detail) > 80:
            return detail[:77] + "..."
        return detail

    def activate_row(self, _listbox, row):
        session_id = getattr(row, "session_id", None)
        if session_id is not None:
            self.activate_session(None, session_id)

    def activate_session(self, _button, session_id):
        try:
            changed = self.bus.activate(session_id)
            write_status(
                self.args.event_file,
                event="activate",
                session=session_id,
                changed=str(changed).lower(),
            )
            self.refresh_sessions()
        except Exception as err:
            write_status(self.args.event_file, event="activate-error", error=err)

    def activate_agent(self, _button, agent, activation_token=""):
        try:
            if _button is None:
                try:
                    self.bus.focus_presentation_window(activation_token)
                except Exception as err:
                    write_status(
                        self.args.event_file,
                        event="focus-presentation-error",
                        agent=agent["id"],
                        error=err,
                    )
            changed = self.bus.activate(agent["session"])
            metadata = agent["metadata"]
            app_id = metadata.get("app_id", "")
            try:
                focus_response = self.bus.focus_window(metadata).strip()
            except Exception as err:
                focus_response = f"focus-window-error:{err}"
            if (
                ("changed=false" in focus_response or focus_response.startswith("focus-window-error:"))
                and app_id
            ):
                try:
                    focus_response = self.bus.focus_app_id(app_id).strip()
                except Exception as err:
                    focus_response = f"focus-app-id-error:{err}"
            self.bus.clear_agent_attention(agent["id"])
            write_status(
                self.args.event_file,
                event="activate-agent",
                agent=agent["id"],
                session=agent["session"],
                changed=str(changed).lower(),
                app_id=app_id or "none",
                focus=focus_response,
            )
            self.refresh_sessions()
        except Exception as err:
            write_status(
                self.args.event_file,
                event="activate-agent-error",
                agent=agent.get("id", "unknown"),
                error=err,
            )

    def handle_notification_action(self, notification_id, action_key):
        if str(action_key) != NOTIFICATION_ACTION_DEFAULT:
            return
        GLib.timeout_add(50, self.activate_notification, int(notification_id))

    def handle_notification_activation_token(self, notification_id, activation_token):
        notification_id = int(notification_id)
        self.notification_tokens[notification_id] = str(activation_token)
        self.activate_notification(notification_id)
        return False

    def activate_notification(self, notification_id):
        if notification_id in self.activated_notifications:
            return False
        agent = self.notification_agents.get(notification_id)
        if agent is None:
            return False
        self.activated_notifications.add(notification_id)
        activation_token = self.notification_tokens.pop(notification_id, "")
        self.activate_agent(None, agent, activation_token)
        return False

    def handle_notification_closed(self, notification_id, _reason):
        notification_id = int(notification_id)
        self.notification_agents.pop(notification_id, None)
        self.notification_tokens.pop(notification_id, None)
        self.activated_notifications.discard(notification_id)

    def activate_session_from_panel_press(self, _gesture, _n_press, x, y):
        self.activate_session_at_position(x, y, event_name="panel-press")

    def activate_session_from_panel_click(self, _gesture, _n_press, x, y):
        self.activate_session_at_position(x, y, event_name="panel-click")

    def handle_legacy_event(self, _controller, event):
        if event is None:
            event = _controller.get_current_event()
        if event is None:
            write_status(self.args.event_file, event="legacy-empty")
            return False
        event_type = event.get_event_type()
        if event_type not in (Gdk.EventType.BUTTON_PRESS, Gdk.EventType.BUTTON_RELEASE):
            return False
        position = event.get_position()
        if isinstance(position, tuple) and len(position) == 3:
            ok, x, y = position
            if not ok:
                return False
        elif isinstance(position, tuple) and len(position) == 2:
            x, y = position
        else:
            return False
        event_name = "legacy-press" if event_type == Gdk.EventType.BUTTON_PRESS else "legacy-release"
        write_status(
            self.args.event_file,
            event=event_name,
            x=round(x, 1),
            y=round(y, 1),
        )
        if event_type == Gdk.EventType.BUTTON_RELEASE:
            self.activate_session_at_position(x, y, event_name=event_name)
        return True

    def activate_session_from_gtk3_click(self, _widget, event):
        self.activate_session_at_position(event.x, event.y, event_name="panel-click")
        return False

    def activate_session_at_position(self, x, y, event_name="panel-click"):
        if y < 54 and x >= max(0, self.args.width - 80):
            write_status(
                self.args.event_file,
                event=f"{event_name}-create",
                x=round(x, 1),
                y=round(y, 1),
            )
            self.create_session()
            return
        list_top = 92
        row_height = 42
        if y < list_top:
            return
        write_status(
            self.args.event_file,
            event=f"{event_name}-ignored-list",
            x=round(x, 1),
            y=round(y, 1),
        )

    def create_session(self, *_args):
        name = self.create_entry.get_text().strip() or None
        try:
            session_id = self.bus.create(name)
            self.create_entry.set_text("")
            write_status(
                self.args.event_file,
                event="create",
                session=session_id,
                name=name or "default",
            )
            self.refresh_sessions()
        except Exception as err:
            write_status(self.args.event_file, event="create-error", error=err)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dbus-name")
    parser.add_argument("--dbus-address")
    parser.add_argument("--width", type=int, default=300)
    parser.add_argument("--refresh-ms", type=int, default=500)
    parser.add_argument("--notification-timeout-ms", type=int, default=300000)
    parser.add_argument("--ready-file")
    parser.add_argument("--event-file")
    args = parser.parse_args()
    return SessionSwitcher(args).run()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as err:
        print(f"ERR {err}", file=sys.stderr)
        raise

#!/usr/bin/env python3
import argparse
import os
import sys

import dbus
import gi


force_gtk3 = os.environ.get("MANIFOLD_SESSION_SWITCHER_FORCE_GTK3") == "1"
disable_adwaita = os.environ.get("MANIFOLD_SESSION_SWITCHER_DISABLE_ADWAITA") == "1"

try:
    if force_gtk3:
        raise ValueError("GTK3 fallback forced")
    gi.require_version("Gdk", "4.0")
    gi.require_version("Gtk", "4.0")
    from gi.repository import Gdk, Gio, GLib, Gtk

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
    from gi.repository import Gdk, Gio, GLib, Gtk

    GTK4 = False


APP_ID = "org.manifold.SessionSwitcher"
DEFAULT_DESTINATION = "org.manifold.Session"


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
        self.filtered_sessions = []
        self.registered = False
        self.last_error = None

    def run(self):
        if not GTK4:
            self.window = self.make_window(None)
            self.window.connect("destroy", Gtk.main_quit)
            self.build_ui()
            self.refresh_sessions()
            self.present_window()
            GLib.timeout_add(250, self.register_with_manifold)
            GLib.timeout_add(self.args.refresh_ms, self.refresh_sessions)
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
        self.refresh_sessions()
        self.present_window()
        GLib.timeout_add(250, self.register_with_manifold)
        GLib.timeout_add(self.args.refresh_ms, self.refresh_sessions)

    def make_window(self, app):
        if GTK4:
            window = Gtk.ApplicationWindow(application=app)
            window.set_default_size(self.args.width, 720)
        else:
            window = Gtk.Window()
            window.set_default_size(self.args.width, 720)
        window.set_title("Manifold Sessions")
        return window

    def present_window(self):
        if GTK4:
            self.window.present()
        else:
            self.window.show_all()

    def build_ui(self):
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
            self.last_error = None
            if sessions != self.sessions:
                self.sessions = sessions
                self.filter_sessions()
        except Exception as err:
            self.last_error = str(err)
            write_status(self.args.event_file, event="refresh-error", error=err)
        return True

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
        self.render_sessions()

    def render_sessions(self):
        self.clear_listbox()
        for session in self.filtered_sessions:
            self.add_session_row(session)
        write_status(
            self.args.event_file,
            event="render",
            sessions=len(self.sessions),
            rows=len(self.filtered_sessions),
        )
        self.present_window()

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
        if session["active"]:
            label = f"* {label}"
        if not GTK4:
            button = Gtk.Button(label=label)
            button.set_size_request(-1, 42)
            button.connect("clicked", self.activate_session, session["id"])
            self.listbox.pack_start(button, False, False, 0)
            button.show_all()
            return

        label_widget = Gtk.Label(label=label, xalign=0)
        label_widget.set_size_request(-1, 42)
        label_widget.set_hexpand(True)
        label_widget.set_margin_start(8)
        label_widget.set_margin_end(8)
        row = Gtk.ListBoxRow()
        row.session_id = session["id"]
        row.set_activatable(True)
        if GTK4:
            click = Gtk.GestureClick()
            click.set_button(0)
            click.connect(
                "pressed",
                lambda _gesture, _n_press, _x, _y, session_id=session["id"]: self.activate_session(
                    None, session_id
                ),
            )
            click.connect(
                "released",
                lambda _gesture, _n_press, _x, _y, session_id=session["id"]: self.activate_session(
                    None, session_id
                ),
            )
            row.add_controller(click)
            row.set_child(label_widget)
        else:
            row.add_events(Gdk.EventMask.BUTTON_RELEASE_MASK)
            row.connect(
                "button-release-event",
                lambda _row, _event, session_id=session["id"]: self.activate_session(
                    None, session_id
                )
                or False,
            )
            label_widget.add_events(Gdk.EventMask.BUTTON_RELEASE_MASK)
            label_widget.connect(
                "button-release-event",
                lambda _label, _event, session_id=session["id"]: self.activate_session(
                    None, session_id
                )
                or False,
            )
            row.add(label_widget)
            row.show_all()
        self.listbox.append(row) if GTK4 else self.listbox.add(row)

    def activate_row(self, _listbox, row):
        self.activate_session(None, row.session_id)

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
        index = int((y - list_top) // row_height)
        if index < 0 or index >= len(self.filtered_sessions):
            return
        session_id = self.filtered_sessions[index]["id"]
        write_status(
            self.args.event_file,
            event=event_name,
            x=round(x, 1),
            y=round(y, 1),
            session=session_id,
        )
        self.activate_session(None, session_id)

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

#!/usr/bin/env python3
import argparse
import os
import sys

import dbus
import dbus.mainloop.glib
import dbus.service
import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk


SERVICE_PATH = "/org/manifold/TestSessionServer"
SERVICE_IFACE = "org.manifold.TestSessionServer"


class TestSessionServer(dbus.service.Object):
    @dbus.service.method(SERVICE_IFACE, in_signature="", out_signature="s")
    def Ping(self):
        return "pong"


def write_file(path, text):
    if not path:
        return
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)


def metadata(**items):
    return dbus.Dictionary(
        {key: str(value) for key, value in items.items()},
        signature="ss",
    )


def register_with_manifold(args):
    address = os.environ.get("MANIFOLDD_DBUS_ADDRESS")
    destination = os.environ.get("MANIFOLDD_DBUS_DESTINATION", "org.manifold.Session")
    if not address:
        raise RuntimeError("MANIFOLDD_DBUS_ADDRESS is required")

    control_bus = dbus.bus.BusConnection(address)
    extension = dbus.Interface(
        control_bus.get_object(destination, "/org/manifold/Extension"),
        "org.manifold.Extension",
    )
    compositor = dbus.Interface(
        control_bus.get_object(destination, "/org/manifold/Compositor"),
        "org.manifold.Compositor",
    )

    extension_id = extension.RegisterExtension(
        "session-switcher",
        metadata(name="python-gtk-session-switcher", purpose="smoke"),
    )
    window_id = compositor.RegisterWindow(
        extension_id,
        metadata(pid=os.getpid()),
        "session-switcher",
    )
    membership_grant = compositor.RequestPresentationMembership(
        window_id,
        metadata(audience="Presentation"),
    )
    placement_grant = compositor.RequestPlacement(
        window_id,
        metadata(mode="Overlay", audience="Presentation", test_only="python-gtk"),
    )
    shortcut_grant = compositor.RequestShortcut(
        extension_id,
        "Ctrl+Alt+Shift+M",
        "Presentation",
        "Python GTK session switcher smoke",
    )
    return control_bus, destination, extension_id, window_id, membership_grant, placement_grant, shortcut_grant


class SwitcherApp:
    def __init__(self, args):
        self.args = args
        self.control_bus = None
        self.control_destination = None
        self.window = None

    def start(self):
        dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
        bus = dbus.SessionBus()
        name = dbus.service.BusName(self.args.service_name, bus)
        self.service = TestSessionServer(bus, SERVICE_PATH)
        self.service_name = name

        GLib.set_prgname("manifold-python-gtk-session-switcher")
        self.window = Gtk.Window(title="Manifold Python GTK Switcher")
        self.window.set_default_size(420, 180)
        self.window.connect("destroy", Gtk.main_quit)

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.set_border_width(12)
        self.window.add(box)
        for session_id, label in self.session_buttons():
            button = Gtk.Button(label=label)
            button.connect("clicked", self.activate_session, session_id)
            box.pack_start(button, True, True, 0)
        self.window.show_all()

        GLib.timeout_add(250, self.finish_registration)
        Gtk.main()

    def finish_registration(self):
        try:
            (
                self.control_bus,
                self.control_destination,
                extension_id,
                window_id,
                membership_grant,
                placement_grant,
                shortcut_grant,
            ) = register_with_manifold(self.args)
            write_file(
                self.args.ready_file,
                "\n".join(
                    [
                        "ready=true",
                        f"service_name={self.args.service_name}",
                        f"pid={os.getpid()}",
                        f"extension_id={extension_id}",
                        f"window_id={window_id}",
                        f"membership_grant={membership_grant}",
                        f"placement_grant={placement_grant}",
                        f"shortcut_grant={shortcut_grant}",
                    ]
                )
                + "\n",
            )
        except Exception as err:
            write_file(self.args.ready_file, f"ready=false\nerror={err}\n")
        return False

    def session_buttons(self):
        if self.args.session_button:
            buttons = []
            for item in self.args.session_button:
                raw_session, sep, raw_label = item.partition(":")
                if not sep:
                    raw_label = f"Activate session {raw_session}"
                buttons.append((int(raw_session), raw_label))
            return buttons
        return [(self.args.target_session, f"Activate session {self.args.target_session}")]

    def activate_session(self, _button, session_id):
        try:
            if self.control_bus is None:
                self.control_bus = dbus.bus.BusConnection(os.environ["MANIFOLDD_DBUS_ADDRESS"])
                self.control_destination = os.environ.get(
                    "MANIFOLDD_DBUS_DESTINATION", "org.manifold.Session"
                )
            session = dbus.Interface(
                self.control_bus.get_object(self.control_destination, "/org/manifold/Session"),
                "org.manifold.Session",
            )
            changed = session.ActivateSurfaceSession(dbus.UInt64(session_id))
            write_file(
                self.args.click_file,
                f"clicked=true\nsession={session_id}\nchanged={bool(changed)}\n",
            )
        except Exception as err:
            write_file(self.args.click_file, f"clicked=false\nerror={err}\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--service-name", default="org.manifold.TestSessionServer")
    parser.add_argument("--target-session", type=int, default=1)
    parser.add_argument(
        "--session-button",
        action="append",
        help="Add a switch button in the form SESSION_ID:LABEL.",
    )
    parser.add_argument("--ready-file", required=True)
    parser.add_argument("--click-file", required=True)
    args = parser.parse_args()
    SwitcherApp(args).start()


if __name__ == "__main__":
    try:
        main()
    except Exception as err:
        print(f"ERR {err}", file=sys.stderr)
        raise

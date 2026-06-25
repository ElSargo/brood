#!/usr/bin/env python3
import argparse
import os
import sys

import dbus
import dbus.mainloop.glib
import dbus.service
import gi

gi.require_version("GLib", "2.0")
from gi.repository import GLib


BASE = "org.manifold.Rube"
IFACE = "org.manifold.Rube.Node"
PATH_BASE = "/org/manifold/Rube"
PROPERTIES = "org.freedesktop.DBus.Properties"
INTROSPECTABLE = "org.freedesktop.DBus.Introspectable"
NOTIFICATIONS = "org.freedesktop.Notifications"


def service_name(node):
    return f"{BASE}.{node}"


def object_path(node):
    return f"{PATH_BASE}/{node}"


def append_event(run_dir, text):
    with open(os.path.join(run_dir, "events.log"), "a", encoding="utf-8") as handle:
        handle.write(text + "\n")


def write_file(path, text):
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)


class RubeNode(dbus.service.Object):
    def __init__(self, bus, node, run_dir, host_bus_address=None):
        self.bus = bus
        self.node = node
        self.run_dir = run_dir
        self.host_bus_address = host_bus_address
        self.props = {
            "Node": node,
            "Value": dbus.Int32(0),
            "LastToken": "",
            "LastDetail": "",
        }
        super().__init__(bus, object_path(node))

    @dbus.service.method(IFACE, in_signature="s", out_signature="s")
    def Kick(self, token):
        self.props["LastToken"] = token
        value = int(self.props["Value"])
        append_event(self.run_dir, f"A Kick token={token} value={value}")
        self.Started(token, value)
        return f"started:{token}:{value}"

    @dbus.service.method(IFACE, in_signature="si", out_signature="i")
    def Transform(self, token, value):
        output = int(value) + 7
        self.props["Value"] = dbus.Int32(output)
        self.props["LastToken"] = token
        self.props["LastDetail"] = f"transformed:{value}->{output}"
        append_event(self.run_dir, f"C Transform token={token} input={value} output={output}")
        self.Transformed(token, output)
        return dbus.Int32(output)

    @dbus.service.method(PROPERTIES, in_signature="ss", out_signature="v")
    def Get(self, interface, prop):
        self._check_interface(interface)
        if prop not in self.props:
            raise dbus.exceptions.DBusException(
                f"unknown property {prop}",
                name="org.freedesktop.DBus.Error.InvalidArgs",
            )
        return self.props[prop]

    @dbus.service.method(PROPERTIES, in_signature="ssv", out_signature="")
    def Set(self, interface, prop, value):
        self._check_interface(interface)
        if prop not in self.props:
            raise dbus.exceptions.DBusException(
                f"unknown property {prop}",
                name="org.freedesktop.DBus.Error.InvalidArgs",
            )
        self.props[prop] = value
        append_event(self.run_dir, f"{self.node} Set {prop}={value}")

    @dbus.service.method(PROPERTIES, in_signature="s", out_signature="a{sv}")
    def GetAll(self, interface):
        self._check_interface(interface)
        return dbus.Dictionary(self.props, signature="sv")

    @dbus.service.signal(IFACE, signature="si")
    def Started(self, token, value):
        pass

    @dbus.service.signal(IFACE, signature="si")
    def Transformed(self, token, value):
        pass

    def _check_interface(self, interface):
        if interface != IFACE:
            raise dbus.exceptions.DBusException(
                f"unsupported interface {interface}",
                name="org.freedesktop.DBus.Error.InvalidArgs",
            )


class SignalBridge:
    def __init__(self, node, bus, run_dir, host_bus_address=None):
        self.node = node
        self.bus = bus
        self.run_dir = run_dir
        self.host_bus_address = host_bus_address
        self.object = RubeNode(bus, node, run_dir, host_bus_address)

        if node == "B":
            bus.add_signal_receiver(
                self.on_started,
                signal_name="Started",
                dbus_interface=IFACE,
                bus_name=service_name("A"),
                path=object_path("A"),
            )
        elif node == "D":
            bus.add_signal_receiver(
                self.on_transformed,
                signal_name="Transformed",
                dbus_interface=IFACE,
                bus_name=service_name("C"),
                path=object_path("C"),
            )

    def on_started(self, token, value):
        alpha = self.bus.get_object(service_name("A"), object_path("A"))
        alpha_xml = dbus.Interface(alpha, INTROSPECTABLE).Introspect()
        alpha_props = dbus.Interface(alpha, PROPERTIES)
        alpha_value = int(alpha_props.Get(IFACE, "Value"))
        self.object.props["LastToken"] = token
        self.object.props["LastDetail"] = f"introspected-A={IFACE in alpha_xml}"
        append_event(
            self.run_dir,
            f"B Started token={token} signal_value={int(value)} prop_value={alpha_value}",
        )

        gamma = dbus.Interface(
            self.bus.get_object(service_name("C"), object_path("C")),
            IFACE,
        )
        result = int(gamma.Transform(token, alpha_value + 1))
        self.object.props["Value"] = dbus.Int32(result)
        append_event(self.run_dir, f"B TransformResult token={token} result={result}")

    def on_transformed(self, token, value):
        gamma = self.bus.get_object(service_name("C"), object_path("C"))
        gamma_props = dbus.Interface(gamma, PROPERTIES)
        last_detail = str(gamma_props.Get(IFACE, "LastDetail"))
        append_event(
            self.run_dir,
            f"D Transformed token={token} value={int(value)} detail={last_detail}",
        )
        notification_id = self.send_notification(token, int(value), last_detail)
        write_file(
            os.path.join(self.run_dir, "done"),
            "\n".join(
                [
                    "done=true",
                    f"token={token}",
                    f"value={int(value)}",
                    f"detail={last_detail}",
                    f"notification_id={notification_id}",
                ]
            )
            + "\n",
        )

    def send_notification(self, token, value, detail):
        if not self.host_bus_address:
            raise RuntimeError("host DBus address is required for the notification hop")
        host_bus = dbus.bus.BusConnection(self.host_bus_address)
        notifications = dbus.Interface(
            host_bus.get_object(NOTIFICATIONS, "/org/freedesktop/Notifications"),
            NOTIFICATIONS,
        )
        return int(
            notifications.Notify(
                "Manifold DBus stress",
                dbus.UInt32(0),
                "",
                "Manifold DBus stress passed",
                f"Rube chain token={token} value={value} {detail}",
                [],
                {},
                dbus.Int32(8000),
            )
        )


def run_node(args):
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus()
    name = dbus.service.BusName(service_name(args.node), bus)
    _ = name
    bridge = SignalBridge(args.node, bus, args.run_dir, args.host_bus_address)
    append_event(args.run_dir, f"{args.node} ready")
    write_file(os.path.join(args.run_dir, f"ready-{args.node}"), f"ready=true\nnode={args.node}\n")
    GLib.timeout_add_seconds(args.timeout, GtklessQuitter().quit)
    GLib.MainLoop().run()
    _ = bridge


class GtklessQuitter:
    def quit(self):
        loop = GLib.MainContext.default()
        while loop.pending():
            loop.iteration(False)
        os._exit(0)


def run_kick(args):
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus()
    seen = []
    for node in ("A", "B", "C", "D"):
        obj = bus.get_object(service_name(node), object_path(node))
        xml = dbus.Interface(obj, INTROSPECTABLE).Introspect()
        if IFACE not in xml or PROPERTIES not in xml:
            raise RuntimeError(f"{node} introspection missing expected interfaces")
        seen.append(node)

    alpha = bus.get_object(service_name("A"), object_path("A"))
    alpha_props = dbus.Interface(alpha, PROPERTIES)
    alpha_props.Set(IFACE, "Value", dbus.Int32(args.value))
    got_value = int(alpha_props.Get(IFACE, "Value"))
    if got_value != args.value:
        raise RuntimeError(f"property roundtrip failed: {got_value} != {args.value}")
    result = dbus.Interface(alpha, IFACE).Kick(args.token)
    write_file(
        os.path.join(args.run_dir, "kick"),
        "\n".join(
            [
                "kick=true",
                f"token={args.token}",
                f"value={args.value}",
                f"introspected={','.join(seen)}",
                f"result={result}",
            ]
        )
        + "\n",
    )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--role", choices=["node", "kick"], required=True)
    parser.add_argument("--node", choices=["A", "B", "C", "D"])
    parser.add_argument("--run-dir", required=True)
    parser.add_argument("--host-bus-address", default=os.environ.get("MANIFOLD_HOST_DBUS_ADDRESS"))
    parser.add_argument("--token", default="stress-token")
    parser.add_argument("--value", type=int, default=41)
    parser.add_argument("--timeout", type=int, default=20)
    args = parser.parse_args()

    if args.role == "node":
        if not args.node:
            raise SystemExit("--node is required for --role node")
        run_node(args)
    else:
        run_kick(args)


if __name__ == "__main__":
    try:
        main()
    except Exception as err:
        print(f"ERR {err}", file=sys.stderr)
        raise

#!/usr/bin/env python3
import argparse
import os

import dbus


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--service-name", default="org.manifold.TestSessionServer")
    parser.add_argument("--out-file", required=True)
    parser.add_argument("--owner-only", action="store_true")
    args = parser.parse_args()

    bus = dbus.SessionBus()
    has_owner = bus.name_has_owner(args.service_name)
    ping = "skipped"
    if has_owner and not args.owner_only:
        service = bus.get_object(args.service_name, "/org/manifold/TestSessionServer")
        iface = dbus.Interface(service, "org.manifold.TestSessionServer")
        ping = str(iface.Ping())
    text = "\n".join(
        [
            f"service_name={args.service_name}",
            f"session_bus_address={os.environ.get('DBUS_SESSION_BUS_ADDRESS', '')}",
            f"has_owner={str(bool(has_owner)).lower()}",
            f"ping={ping}",
        ]
    )
    with open(args.out_file, "w", encoding="utf-8") as handle:
        handle.write(text + "\n")
    print(text)


if __name__ == "__main__":
    main()

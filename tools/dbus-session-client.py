#!/usr/bin/env python3
import argparse

import dbus


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--service-name", default="org.manifold.TestSessionServer")
    parser.add_argument("--out-file", required=True)
    args = parser.parse_args()

    bus = dbus.SessionBus()
    has_owner = bus.name_has_owner(args.service_name)
    text = "\n".join(
        [
            f"service_name={args.service_name}",
            f"has_owner={str(bool(has_owner)).lower()}",
        ]
    )
    with open(args.out_file, "w", encoding="utf-8") as handle:
        handle.write(text + "\n")
    print(text)


if __name__ == "__main__":
    main()
